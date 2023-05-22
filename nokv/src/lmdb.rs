use libc::{c_char, c_int, c_uint, c_void, size_t, EINVAL};
pub use lmdb_master_sys as ffi;
use parking_lot::RwLock;
use std::{
    cmp::Ordering,
    collections::HashMap,
    ffi::{CStr, CString, NulError},
    fs, mem,
    ops::{Bound, Deref},
    path::Path,
    ptr,
    rc::Rc,
    slice,
    sync::Arc,
};

macro_rules! lmdb_try {
    ($expr:expr) => {{
        match $expr {
            ffi::MDB_SUCCESS => (),
            err_code => return Err(lmdb_error(err_code)),
        }
    }};
}

macro_rules! lmdb_try_with_cleanup {
    ($expr:expr, $cleanup:expr) => {{
        match $expr {
            ffi::MDB_SUCCESS => (),
            err_code => {
                let _ = $cleanup;
                return Err(lmdb_error(err_code));
            }
        }
    }};
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum Error {
    #[error(transparent)]
    CString(#[from] NulError),
    #[error("Lmdb error: {0}")]
    Message(String),
    #[error("need drop iterators from writer first")]
    Commit,
}

type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(Debug)]
pub struct Slice {
    inner: ffi::MDB_val,
}

impl Slice {
    pub unsafe fn inner(&self) -> &[u8] {
        slice::from_raw_parts(self.inner.mv_data as *const u8, self.inner.mv_size as usize)
    }
}

impl AsRef<[u8]> for Slice {
    fn as_ref(&self) -> &[u8] {
        unsafe { self.inner() }
    }
}

impl Deref for Slice {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        unsafe { self.inner() }
    }
}

struct Dbi {
    inner: ffi::MDB_dbi,
}

impl Dbi {
    fn new(txn: *mut ffi::MDB_txn, name: Option<&str>, flags: c_uint) -> Result<Self> {
        let c_name = name.map(|n| CString::new(n)).transpose()?;
        let name_ptr = if let Some(ref c_name) = c_name {
            c_name.as_ptr()
        } else {
            ptr::null()
        };
        let mut dbi: ffi::MDB_dbi = 0;
        unsafe {
            lmdb_result(ffi::mdb_dbi_open(txn, name_ptr, flags, &mut dbi))?;
        }
        Ok(Self { inner: dbi })
    }
}

#[derive(Debug, Clone)]
pub struct Tree {
    inner: ffi::MDB_dbi,
    flags: c_uint,
}

unsafe impl Send for Tree {}
unsafe impl Sync for Tree {}

impl Tree {}

struct Txn {
    _db: Option<Arc<DbInner>>,
    inner: *mut ffi::MDB_txn,
}

impl Txn {
    fn new_ro(db: &DbInner, _db: Option<Arc<DbInner>>) -> Result<Self> {
        let mut txn: *mut ffi::MDB_txn = ptr::null_mut();
        unsafe {
            lmdb_result(ffi::mdb_txn_begin(
                db.inner,
                ptr::null_mut(),
                ffi::MDB_RDONLY,
                &mut txn,
            ))?;
        }
        Ok(Self { inner: txn, _db })
    }

    fn new_rw(db: &DbInner, _db: Option<Arc<DbInner>>) -> Result<Self> {
        let mut txn: *mut ffi::MDB_txn = ptr::null_mut();
        unsafe {
            lmdb_result(ffi::mdb_txn_begin(db.inner, ptr::null_mut(), 0, &mut txn))?;
        }
        Ok(Self { inner: txn, _db })
    }

    fn commit(self) -> Result<()> {
        unsafe {
            let result = lmdb_result(ffi::mdb_txn_commit(self.inner));
            mem::forget(self);
            result
        }
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        unsafe { ffi::mdb_txn_abort(self.inner) }
    }
}

pub struct Reader {
    txn: Rc<Txn>,
}

impl Reader {
    fn new(db: &Arc<DbInner>) -> Result<Self> {
        // let txn = Txn::new_ro(db, Some(Arc::clone(db)))?;
        let txn = Txn::new_ro(db, None)?;
        Ok(Self { txn: Rc::new(txn) })
    }

    pub fn get<K: AsRef<[u8]>>(&self, tree: &Tree, key: K) -> Result<Option<Slice>> {
        get_in_txn(self.txn.inner, tree, key)
    }

    pub fn iter_from<K: AsRef<[u8]>>(&self, tree: &Tree, from: Bound<K>, rev: bool) -> Iter {
        let mut iter = Iter::new(Rc::clone(&self.txn), tree);
        iter.seek(from, rev);
        iter
    }

    pub fn iter(&self, tree: &Tree) -> Iter {
        self.iter_from(tree, Bound::Unbounded::<Vec<u8>>, false)
    }
}

pub struct Writer {
    txn: Rc<Txn>,
}

fn get_in_txn<K: AsRef<[u8]>>(
    txn: *mut ffi::MDB_txn,
    tree: &Tree,
    key: K,
) -> Result<Option<Slice>> {
    let key = key.as_ref();
    let mut key_val = ffi::MDB_val {
        mv_size: key.len() as size_t,
        mv_data: key.as_ptr() as *mut c_void,
    };

    let mut data_val = ffi::MDB_val {
        mv_size: 0,
        mv_data: ptr::null_mut(),
    };
    unsafe {
        match ffi::mdb_get(txn, tree.inner, &mut key_val, &mut data_val) {
            ffi::MDB_SUCCESS => Ok(Some(Slice { inner: data_val })),
            ffi::MDB_NOTFOUND => Ok(None),
            err_code => Err(lmdb_error(err_code)),
        }
    }
}

impl Writer {
    fn new(db: &Arc<DbInner>) -> Result<Self> {
        let txn = Txn::new_rw(db, Some(Arc::clone(db)))?;
        Ok(Self { txn: Rc::new(txn) })
    }

    pub fn get<K: AsRef<[u8]>>(&self, tree: &Tree, key: K) -> Result<Option<Slice>> {
        get_in_txn(self.txn.inner, tree, key)
    }

    pub fn iter_from<K: AsRef<[u8]>>(&self, tree: &Tree, from: Bound<K>, rev: bool) -> Iter {
        let mut iter = Iter::new(Rc::clone(&self.txn), tree);
        iter.seek(from, rev);
        iter
    }

    pub fn iter(&self, tree: &Tree) -> Iter {
        self.iter_from(tree, Bound::Unbounded::<Vec<u8>>, false)
    }

    pub fn put<K, V>(&mut self, tree: &Tree, key: K, value: V) -> Result<()>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let flags = 0;
        let key = key.as_ref();
        let value = value.as_ref();

        let mut key_val: ffi::MDB_val = ffi::MDB_val {
            mv_size: key.len() as size_t,
            mv_data: key.as_ptr() as *mut c_void,
        };
        let mut data_val: ffi::MDB_val = ffi::MDB_val {
            mv_size: value.len() as size_t,
            mv_data: value.as_ptr() as *mut c_void,
        };
        unsafe {
            lmdb_result(ffi::mdb_put(
                self.txn.inner,
                tree.inner,
                &mut key_val,
                &mut data_val,
                flags,
            ))
        }
    }

    pub fn del<K: AsRef<[u8]>>(&mut self, tree: &Tree, key: K, value: Option<&[u8]>) -> Result<()> {
        let key = key.as_ref();
        let mut key_val: ffi::MDB_val = ffi::MDB_val {
            mv_size: key.len() as size_t,
            mv_data: key.as_ptr() as *mut c_void,
        };

        if let Some(value) = value {
            let mut data_val = ffi::MDB_val {
                mv_size: value.len() as size_t,
                mv_data: value.as_ptr() as *mut c_void,
            };
            unsafe {
                match ffi::mdb_del(self.txn.inner, tree.inner, &mut key_val, &mut data_val) {
                    ffi::MDB_SUCCESS | ffi::MDB_NOTFOUND => Ok(()),
                    err_code => Err(lmdb_error(err_code)),
                }
            }
        } else {
            unsafe {
                match ffi::mdb_del(self.txn.inner, tree.inner, &mut key_val, ptr::null_mut()) {
                    ffi::MDB_SUCCESS | ffi::MDB_NOTFOUND => Ok(()),
                    err_code => Err(lmdb_error(err_code)),
                }
            }
        }
    }

    pub fn commit(self) -> Result<()> {
        match Rc::try_unwrap(self.txn) {
            Ok(txn) => txn.commit(),
            Err(_) => Err(Error::Commit),
        }
    }
}

fn to_cpath<P: AsRef<Path>>(path: P) -> Result<CString, Error> {
    Ok(CString::new(path.as_ref().to_string_lossy().as_bytes())?)
}

struct DbInner {
    inner: *mut ffi::MDB_env,
    dbs: RwLock<HashMap<Option<String>, Dbi>>,
}

impl Drop for DbInner {
    fn drop(&mut self) {
        unsafe { ffi::mdb_env_close(self.inner) }
    }
}

impl DbInner {
    fn open<P: AsRef<Path>>(
        path: P,
        maxdbs: Option<u32>,
        maxreaders: Option<u32>,
        mapsize: Option<usize>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let c_path = to_cpath(path)?;

        if let Err(e) = fs::create_dir_all(path) {
            return Err(Error::Message(format!(
                "Failed to create LMDB directory: `{e:?}`."
            )));
        }

        let mut env: *mut ffi::MDB_env = ptr::null_mut();
        // let flag = ffi::MDB_NOTLS;
        let flag = 0;
        unsafe {
            lmdb_try!(ffi::mdb_env_create(&mut env));

            if let Some(maxdbs) = maxdbs {
                lmdb_try_with_cleanup!(
                    ffi::mdb_env_set_maxdbs(env, maxdbs),
                    ffi::mdb_env_close(env)
                );
            }

            if let Some(maxreaders) = maxreaders {
                lmdb_try_with_cleanup!(
                    ffi::mdb_env_set_maxreaders(env, maxreaders),
                    ffi::mdb_env_close(env)
                );
            }

            if let Some(mapsize) = mapsize {
                lmdb_try_with_cleanup!(
                    ffi::mdb_env_set_mapsize(env, mapsize),
                    ffi::mdb_env_close(env)
                );
            }

            lmdb_try_with_cleanup!(
                ffi::mdb_env_open(env, c_path.as_ptr(), flag, 0o644),
                ffi::mdb_env_close(env)
            );
        }

        Ok(Self {
            inner: env,
            dbs: RwLock::new(HashMap::new()),
        })
    }

    fn open_tree(&self, name: Option<&str>, flags: c_uint) -> Result<Tree> {
        let sname = name.map(ToOwned::to_owned);
        {
            let dbs = self.dbs.read();
            if let Some(dbi) = dbs.get(&sname) {
                return Ok(Tree {
                    flags,
                    inner: dbi.inner,
                });
            }
        }

        // we need to check this again in case another
        // thread opened it concurrently.
        let mut dbs = self.dbs.write();
        if let Some(dbi) = dbs.get(&sname) {
            return Ok(Tree {
                flags,
                inner: dbi.inner,
            });
        }

        // create
        let writer = Txn::new_rw(self, None)?;
        let flags = ffi::MDB_CREATE | flags;

        let dbi = Dbi::new(writer.inner, name, flags)?;
        let inner = dbi.inner;
        writer.commit()?;
        dbs.insert(sname, dbi);
        Ok(Tree { flags, inner })
    }

    fn drop_tree(&self, name: Option<&str>) -> Result<bool> {
        // let sname = name.to_string();
        if let Some(dbi) = self.dbs.write().remove(&name.map(|s| s.to_owned())) {
            let writer = Txn::new_rw(self, None)?;
            unsafe {
                lmdb_result(ffi::mdb_drop(writer.inner, dbi.inner, 1))?;
            }
            writer.commit()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub struct Db {
    inner: Arc<DbInner>,
}

unsafe impl Send for Db {}
unsafe impl Sync for Db {}

impl Db {
    pub fn writer(&self) -> Result<Writer> {
        Writer::new(&self.inner)
    }

    pub fn open_tree(&self, name: Option<&str>, flags: c_uint) -> std::result::Result<Tree, Error> {
        self.inner.open_tree(name, flags)
    }

    pub fn drop_tree(&self, name: Option<&str>) -> std::result::Result<bool, Error> {
        self.inner.drop_tree(name)
    }

    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with(path, Some(20), Some(100), Some(1_000_000_000_000))
    }

    pub fn open_with<P: AsRef<Path>>(
        path: P,
        maxdbs: Option<u32>,
        maxreaders: Option<u32>,
        mapsize: Option<usize>,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(DbInner::open(path, maxdbs, maxreaders, mapsize)?),
        })
    }

    pub fn reader(&self) -> std::result::Result<Reader, Error> {
        Reader::new(&self.inner)
    }

    pub fn flush(&self) -> Result<()> {
        unsafe {
            lmdb_result(ffi::mdb_env_sync(self.inner.inner, 1))?;
        }
        Ok(())
    }
}

pub struct Iter {
    err: Option<Error>,
    inner: Option<InnerIter>,
    rev: bool,
    op: c_uint,
    next_op: c_uint,
    dup: bool,
}

impl Iter {
    fn new(txn: Rc<Txn>, tree: &Tree) -> Self {
        let dup = tree.flags & ffi::MDB_DUPSORT == ffi::MDB_DUPSORT;

        let inner = InnerIter::new(txn, tree.inner);
        match inner {
            Err(err) => Self {
                err: Some(err),
                inner: None,
                rev: false,
                op: 0,
                next_op: 0,
                dup,
            },
            Ok(inner) => Self {
                err: None,
                inner: Some(inner),
                rev: false,
                op: 0,
                next_op: 0,
                dup,
            },
        }
    }
}

impl Iter {
    pub fn seek<K: AsRef<[u8]>>(&mut self, from: Bound<K>, rev: bool) {
        self.rev = rev;
        if let Some(ref mut inner) = self.inner {
            if rev {
                self.next_op = ffi::MDB_PREV;
                match from {
                    Bound::Included(start) => {
                        self.op = ffi::MDB_GET_CURRENT;
                        match inner.get_by_key(start.as_ref(), ffi::MDB_SET_RANGE) {
                            Ok(Some((key, _))) => {
                                let cmp = key.deref().cmp(start.as_ref());
                                match cmp {
                                    Ordering::Greater => {
                                        self.op = ffi::MDB_PREV;
                                    }
                                    Ordering::Equal if self.dup => {
                                        // move to last value if the same key
                                        // MDB_LAST_DUP will not return key
                                        // self.op = ffi::MDB_LAST_DUP;
                                        let _r = inner.get(ffi::MDB_LAST_DUP);
                                    }
                                    _ => {}
                                };
                            }
                            Ok(None) => {
                                // bigger than all
                                self.op = ffi::MDB_LAST;
                            }
                            Err(err) => {
                                self.err = Some(err);
                            }
                        }
                    }
                    Bound::Excluded(start) => {
                        self.op = ffi::MDB_GET_CURRENT;
                        match inner.get_by_key(start.as_ref(), ffi::MDB_SET_RANGE) {
                            Ok(Some((key, _))) => {
                                if key.deref() >= start.as_ref() {
                                    if self.dup {
                                        self.op = ffi::MDB_PREV_NODUP;
                                    } else {
                                        self.op = ffi::MDB_PREV;
                                    }
                                }
                            }
                            Ok(None) => {
                                // bigger than all
                                self.op = ffi::MDB_LAST;
                            }
                            Err(err) => {
                                self.err = Some(err);
                            }
                        }
                    }
                    Bound::Unbounded => {
                        self.op = ffi::MDB_LAST;
                    }
                };
            } else {
                self.next_op = ffi::MDB_NEXT;
                match from {
                    Bound::Included(start) => {
                        self.op = ffi::MDB_GET_CURRENT;
                        match inner.get_by_key(start.as_ref(), ffi::MDB_SET_RANGE) {
                            Err(err) => {
                                self.err = Some(err);
                            }
                            _ => {}
                        }
                    }
                    Bound::Excluded(start) => {
                        self.op = ffi::MDB_GET_CURRENT;
                        match inner.get_by_key(start.as_ref(), ffi::MDB_SET_RANGE) {
                            Ok(Some((key, _))) => {
                                if start.as_ref() == key.deref() {
                                    if self.dup {
                                        self.op = ffi::MDB_NEXT_NODUP;
                                    } else {
                                        self.op = ffi::MDB_NEXT;
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                self.err = Some(err);
                            }
                        }
                    }
                    Bound::Unbounded => {
                        self.op = ffi::MDB_FIRST;
                    }
                };
            }
        }
    }
}

impl Iterator for Iter {
    type Item = Result<(Slice, Slice), Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(err) = &self.err {
            Some(Err(err.clone()))
        } else if let Some(ref mut inner) = self.inner {
            let item = inner.get(self.op);
            self.op = self.next_op;
            item.transpose()
        } else {
            None
        }
    }
}

fn lmdb_error(err_code: c_int) -> Error {
    unsafe {
        // This is safe since the error messages returned from mdb_strerror are static.
        let err: *const c_char = ffi::mdb_strerror(err_code) as *const c_char;
        Error::Message(std::str::from_utf8_unchecked(CStr::from_ptr(err).to_bytes()).to_string())
    }
}

fn lmdb_result(err_code: c_int) -> Result<()> {
    if err_code == ffi::MDB_SUCCESS {
        Ok(())
    } else {
        Err(lmdb_error(err_code))
    }
}

// unsafe fn slice_to_val(slice: Option<&[u8]>) -> ffi::MDB_val {
//     match slice {
//         Some(slice) => ffi::MDB_val {
//             mv_size: slice.len() as size_t,
//             mv_data: slice.as_ptr() as *mut c_void,
//         },
//         None => ffi::MDB_val {
//             mv_size: 0,
//             mv_data: ptr::null_mut(),
//         },
//     }
// }

// unsafe fn val_to_slice<'a>(val: ffi::MDB_val) -> &'a [u8] {
//     slice::from_raw_parts(val.mv_data as *const u8, val.mv_size as usize)
// }

/// copy from [`lmdb::Cursor`]
struct InnerIter {
    _txn: Rc<Txn>,
    cursor: *mut ffi::MDB_cursor,
}

type InnerItem = Result<Option<(Slice, Slice)>, Error>;

impl InnerIter {
    fn new(_txn: Rc<Txn>, dbi: ffi::MDB_dbi) -> Result<Self, Error> {
        let mut cursor: *mut ffi::MDB_cursor = ptr::null_mut();
        unsafe {
            lmdb_result(ffi::mdb_cursor_open(_txn.inner, dbi, &mut cursor))?;
        }
        Ok(Self { cursor, _txn })
    }

    fn get_by_key(&mut self, key: &[u8], op: c_uint) -> InnerItem {
        let key = ffi::MDB_val {
            mv_size: key.len() as size_t,
            mv_data: key.as_ptr() as *mut c_void,
        };
        self.get_by_mdb_key(key, op)
    }

    fn get_by_mdb_key(&mut self, mut key: ffi::MDB_val, op: c_uint) -> InnerItem {
        let mut data = ffi::MDB_val {
            mv_size: 0,
            mv_data: ptr::null_mut(),
        };
        unsafe {
            match ffi::mdb_cursor_get(self.cursor, &mut key, &mut data, op) {
                ffi::MDB_SUCCESS => {
                    let k = Slice { inner: key };
                    let v = Slice { inner: data };
                    Ok(Some((k, v)))
                }
                // EINVAL can occur when the cursor was previously seeked to a non-existent value,
                // e.g. iter_from with a key greater than all values in the database.
                ffi::MDB_NOTFOUND | EINVAL => Ok(None),
                error => Err(lmdb_error(error)),
            }
        }
    }

    fn get(&mut self, op: c_uint) -> InnerItem {
        let key = ffi::MDB_val {
            mv_size: 0,
            mv_data: ptr::null_mut(),
        };
        self.get_by_mdb_key(key, op)
    }
}

impl Drop for InnerIter {
    fn drop(&mut self) {
        unsafe {
            ffi::mdb_cursor_close(self.cursor);
        }
    }
}
