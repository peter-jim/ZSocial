use crate::{
    error::Error,
    key::{concat, concat_sep, encode_replace_key, u64_to_ver, IndexKey},
    ArchivedEventIndex, Event, EventIndex, Filter, FromEventData, Stats,
};
use nostr_kv::{
    lmdb::{Db as Lmdb, Iter as LmdbIter, *},
    scanner::{Group, MatchResult, Scanner},
};

use std::{
    marker::PhantomData,
    ops::Bound,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

type Result<T, E = Error> = core::result::Result<T, E>;

pub fn upper(mut key: Vec<u8>) -> Option<Vec<u8>> {
    while let Some(last) = key.pop() {
        if last < u8::max_value() {
            key.push(last + 1);
            return Some(key);
        }
    }
    None
}

const MAX_TAG_VALUE_SIZE: usize = 255;

#[derive(Clone)]
pub struct Db {
    inner: Lmdb,
    // save data
    t_data: Tree,
    // save index
    t_index: Tree,
    // map id to uid
    t_id_uid: Tree,
    // map id to word
    t_uid_word: Tree,
    // id time
    t_id: Tree,
    // pubkey time
    t_pubkey: Tree,
    // kind time
    t_kind: Tree,
    t_pubkey_kind: Tree,
    t_created_at: Tree,
    t_tag: Tree,
    t_deletion: Tree,
    t_replacement: Tree,
    t_expiration: Tree,
    // word time
    t_word: Tree,
    seq: Arc<AtomicU64>,
}

fn u64_from_bytes(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(bytes.try_into()?))
}

// Get the latest seq from db
fn latest_seq(db: &Lmdb, tree: &Tree) -> Result<u64, Error> {
    let txn = db.reader()?;
    let mut iter = txn.iter_from(tree, Bound::Unbounded::<Vec<u8>>, true);
    if let Some(item) = iter.next() {
        let (k, _) = item?;
        u64_from_bytes(k.as_ref())
    } else {
        Ok(0)
    }
}

#[cfg(feature = "zstd")]
fn encode_event(event: &Event) -> Result<Vec<u8>> {
    let json = event.to_json()?;
    let mut json = zstd::encode_all(json.as_bytes(), 5).map_err(|e| Error::Io(e))?;
    json.push(1);
    Ok(json)
}
#[cfg(not(feature = "zstd"))]
fn encode_event(event: &Event) -> Result<String> {
    event.to_json()
}

impl Db {
    fn del_event(&self, writer: &mut Writer, event: &Event, uid: &[u8]) -> Result<(), Error> {
        let index_event = event.index();
        let time = index_event.created_at();
        let kind = index_event.kind();
        let pubkey = index_event.pubkey();

        // word
        let bytes = writer.get(&self.t_uid_word, uid)?;
        if let Some(bytes) = bytes {
            let bytes = bytes.to_vec();
            writer.del(&self.t_uid_word, uid, None)?;
            let word = unsafe { rkyv::archived_root::<Vec<Vec<u8>>>(&bytes) };
            for item in word.as_slice() {
                writer.del(&self.t_word, IndexKey::encode_word(item, time), Some(uid))?;
            }
        }

        writer.del(&self.t_data, uid, None)?;
        writer.del(&self.t_index, uid, None)?;
        writer.del(&self.t_id_uid, index_event.id(), None)?;

        writer.del(
            &self.t_id,
            IndexKey::encode_id(index_event.id(), time),
            Some(uid),
        )?;

        writer.del(&self.t_kind, IndexKey::encode_kind(kind, time), Some(uid))?;

        writer.del(
            &self.t_pubkey,
            IndexKey::encode_pubkey(pubkey, time),
            Some(uid),
        )?;
        writer.del(
            &self.t_pubkey_kind,
            IndexKey::encode_pubkey_kind(pubkey, kind, time),
            Some(uid),
        )?;

        if let Some(delegator) = index_event.delegator() {
            writer.del(
                &self.t_pubkey,
                IndexKey::encode_pubkey(delegator, time),
                Some(uid),
            )?;
            writer.del(
                &self.t_pubkey_kind,
                IndexKey::encode_pubkey_kind(delegator, kind, time),
                Some(uid),
            )?;
        }

        writer.del(&self.t_created_at, IndexKey::encode_time(time), Some(uid))?;

        let tagval = concat(uid, kind.to_be_bytes());
        for tag in index_event.tags() {
            writer.del(
                &self.t_tag,
                IndexKey::encode_tag(&tag.0, &tag.1, time),
                Some(&tagval),
            )?;
        }

        // replacement index
        if let Some(k) = encode_replace_key(index_event.kind(), index_event.pubkey(), event.tags())
        {
            writer.del(&self.t_replacement, k, None)?;
        }

        // expiration
        if let Some(t) = index_event.expiration() {
            writer.del(&self.t_expiration, IndexKey::encode_time(*t), Some(uid))?;
        }

        Ok(())
    }

    fn put_event(
        &self,
        writer: &mut Writer,
        event: &Event,
        uid: &Vec<u8>,
        replace_key: &Option<Vec<u8>>,
    ) -> Result<(), Error> {
        let index_event = event.index();

        // put event
        let time = index_event.created_at();
        let json = encode_event(&event)?;

        writer.put(&self.t_data, uid, json)?;

        // put index
        let bytes = index_event.to_bytes()?;
        writer.put(&self.t_index, uid, bytes)?;

        // put view
        let kind = index_event.kind();
        let pubkey = index_event.pubkey();

        writer.put(&self.t_id_uid, index_event.id(), uid)?;

        writer.put(&self.t_id, IndexKey::encode_id(index_event.id(), time), uid)?;

        writer.put(&self.t_kind, IndexKey::encode_kind(kind, time), uid)?;

        writer.put(&self.t_pubkey, IndexKey::encode_pubkey(pubkey, time), uid)?;
        writer.put(
            &self.t_pubkey_kind,
            IndexKey::encode_pubkey_kind(pubkey, kind, time),
            uid,
        )?;

        if let Some(delegator) = index_event.delegator() {
            writer.put(
                &self.t_pubkey,
                IndexKey::encode_pubkey(delegator, time),
                uid,
            )?;
            writer.put(
                &self.t_pubkey_kind,
                IndexKey::encode_pubkey_kind(delegator, kind, time),
                uid,
            )?;
        }

        writer.put(&self.t_created_at, IndexKey::encode_time(time), uid)?;

        let tagval = concat(uid, kind.to_be_bytes());
        for tag in index_event.tags() {
            let key = &tag.0;
            let v = &tag.1;
            // tag[0] == 'e'
            if kind == 5 && key[0] == 101 {
                writer.put(&self.t_deletion, concat(index_event.id(), v), uid)?;
            }
            // Provide pubkey kind for filter
            writer.put(&self.t_tag, IndexKey::encode_tag(key, v, time), &tagval)?;
        }

        // replacement index
        if let Some(k) = replace_key {
            // writer.put(&self.t_replacement, k, concat(time.to_be_bytes(), uid))?;
            writer.put(&self.t_replacement, k, uid)?;
        }

        // expiration
        if let Some(t) = index_event.expiration() {
            writer.put(&self.t_expiration, IndexKey::encode_time(*t), uid)?;
        }

        // word
        if let Some(word) = &event.words {
            let bytes =
                rkyv::to_bytes::<_, 256>(word).map_err(|e| Error::Serialization(e.to_string()))?;
            writer.put(&self.t_uid_word, uid, bytes)?;
            for item in word {
                writer.put(&self.t_word, IndexKey::encode_word(item, time), uid)?;
            }
        }
        Ok(())
    }
}

fn get_event<R: FromEventData, K: AsRef<[u8]>, T: Transaction>(
    reader: &T,
    id_tree: &Tree,
    data_tree: &Tree,
    event_id: K,
) -> Result<Option<(Vec<u8>, R)>, Error> {
    let uid = get_uid(reader, id_tree, event_id)?;
    if let Some(uid) = uid {
        let event = get_event_by_uid(reader, data_tree, &uid)?;
        if let Some(event) = event {
            return Ok(Some((uid, event)));
        }
    }
    Ok(None)
}

fn get_event_by_uid<R: FromEventData, K: AsRef<[u8]>, T: Transaction>(
    reader: &T,
    data_tree: &Tree,
    uid: K,
) -> Result<Option<R>, Error> {
    let v = reader.get(&data_tree, uid)?;
    if let Some(v) = v {
        return Ok(Some(
            R::from_data(v.as_ref()).map_err(|e| Error::Message(e.to_string()))?,
        ));
    }
    Ok(None)
}

fn get_uid<K: AsRef<[u8]>, T: Transaction>(
    reader: &T,
    id_tree: &Tree,
    event_id: K,
) -> Result<Option<Vec<u8>>, Error> {
    Ok(reader
        .get(&id_tree, event_id.as_ref())?
        .map(|v| v.as_ref().to_vec()))
    // let mut iter = reader.iter_from(id_tree, Bound::Included(event_id.as_ref()), false);
    // if let Some(item) = iter.next() {
    //     let (k, v, _) = item?;
    //     if k.as_ref().starts_with(event_id.as_ref()) {
    //         return Ok(Some(v.as_ref().to_vec()));
    //     }
    // }
    // Ok(None)
}

#[derive(Debug, Clone)]
pub enum CheckEventResult {
    Invald(String),
    Duplicate,
    Deleted,
    ReplaceIgnored,
    Ok(usize),
}

impl Db {
    pub fn flush(&self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }

    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let inner = Lmdb::open(path)?;

        let default_opts = 0;
        let integer_default_opts = ffi::MDB_INTEGERKEY;

        let index_opts = ffi::MDB_DUPSORT | ffi::MDB_DUPFIXED | ffi::MDB_INTEGERDUP;

        let integer_index_opts =
            ffi::MDB_DUPSORT | ffi::MDB_INTEGERKEY | ffi::MDB_DUPFIXED | ffi::MDB_INTEGERDUP;

        let view_data = inner.open_tree(Some("t_data"), integer_default_opts)?;
        Ok(Self {
            seq: Arc::new(AtomicU64::new(latest_seq(&inner, &view_data)?)),
            t_data: view_data,
            t_index: inner.open_tree(Some("t_index"), integer_default_opts)?,
            t_id_uid: inner.open_tree(Some("t_id_uid"), default_opts)?,
            t_uid_word: inner.open_tree(Some("t_uid_word"), default_opts)?,
            t_deletion: inner.open_tree(Some("t_deletion"), default_opts)?,
            t_replacement: inner.open_tree(Some("t_replacement"), default_opts)?,
            t_id: inner.open_tree(Some("t_id"), default_opts)?,
            t_pubkey: inner.open_tree(Some("t_pubkey"), index_opts)?,
            t_kind: inner.open_tree(Some("t_kind"), index_opts)?,
            t_pubkey_kind: inner.open_tree(Some("t_pubkey_kind"), index_opts)?,
            t_created_at: inner.open_tree(Some("t_created_at"), integer_index_opts)?,
            t_tag: inner.open_tree(Some("t_tag"), ffi::MDB_DUPSORT | ffi::MDB_DUPFIXED)?,
            t_expiration: inner.open_tree(Some("t_expiration"), integer_index_opts)?,
            t_word: inner.open_tree(Some("t_word"), index_opts)?,

            inner,
        })
    }

    pub fn writer<'env>(&'env self) -> Result<Writer<'env>> {
        Ok(self.inner.writer()?)
    }

    pub fn reader<'env>(&'env self) -> Result<Reader<'env>> {
        Ok(self.inner.reader()?)
    }

    pub fn commit<T: Transaction>(&self, txn: T) -> Result<()> {
        Ok(txn.commit()?)
    }

    pub fn put<E: AsRef<Event>>(&self, writer: &mut Writer, event: E) -> Result<CheckEventResult> {
        let event = event.as_ref();
        let mut count = 0;

        if event.id().len() != 32 || event.pubkey().len() != 32 {
            return Ok(CheckEventResult::Invald(
                "invalid event id or pubkey".to_owned(),
            ));
        }
        // let id: Vec<u8> = pad_start(event.id(), 32);
        let event_id = event.id();
        let pubkey = event.pubkey();

        // Check duplicate event.
        {
            // dup in the db.
            if get_uid(writer, &self.t_id_uid, event_id)?.is_some() {
                return Ok(CheckEventResult::Duplicate);
            }
        }

        // check deleted in db
        if writer
            .get(&self.t_deletion, concat(&event_id, pubkey))?
            .is_some()
        {
            return Ok(CheckEventResult::Deleted);
        }

        // [NIP-09](https://nips.be/9)
        // delete event
        if event.kind() == 5 {
            for tag in event.index().tags() {
                if tag.0 == b"e" {
                    // let key = hex::decode(&tag.1).map_err(|e| Error::Hex(e))?;
                    let key = &tag.1;
                    let r = get_event::<Event, _, _>(writer, &self.t_id_uid, &self.t_data, key)?;
                    if let Some((uid, e)) = r {
                        // check author or deletion event
                        // check delegator
                        if (e.pubkey() == event.pubkey()
                            || e.index().delegator() == Some(event.pubkey()))
                            && e.kind() != 5
                        {
                            count += 1;
                            self.del_event(writer, &e, &uid)?;
                        }
                    }
                }
            }
        }

        // check replacement event
        let replace_key = encode_replace_key(event.kind(), event.pubkey(), event.tags());

        if let Some(replace_key) = replace_key.as_ref() {
            // lmdb max_key_size 511 bytes
            // we only index tag value length < 255
            if replace_key.len() > MAX_TAG_VALUE_SIZE + 8 + 32 {
                return Ok(CheckEventResult::Invald("invalid replace key".to_owned()));
            }

            // replace in the db
            let v = writer.get(&self.t_replacement, &replace_key)?;
            if let Some(v) = v {
                let uid = v.to_vec();
                // let t = &v[0..8];
                // let t = u64_from_bytes(t);
                // if event.created_at() < t {
                //     continue;
                // }
                let e: Option<Event> = get_event_by_uid(writer, &self.t_data, &uid)?;
                if let Some(e) = e {
                    if event.created_at() < e.created_at() {
                        return Ok(CheckEventResult::ReplaceIgnored);
                    }
                    // del old
                    count += 1;
                    self.del_event(writer, &e, &uid)?;
                }
            }
        }

        count += 1;

        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let seq = u64_to_ver(seq);
        self.put_event(writer, event, &seq, &replace_key)?;
        Ok(CheckEventResult::Ok(count))
    }

    pub fn get<R: FromEventData, K: AsRef<[u8]>, T: Transaction>(
        &self,
        txn: &T,
        event_id: K,
    ) -> Result<Option<R>> {
        let event = get_event(txn, &self.t_id_uid, &self.t_data, event_id)?;
        Ok(event.map(|e| e.1))
    }

    pub fn del<K: AsRef<[u8]>>(&self, writer: &mut Writer, event_id: K) -> Result<bool> {
        if let Some((uid, event)) =
            get_event::<Event, _, _>(writer, &self.t_id_uid, &self.t_data, event_id.as_ref())?
        {
            self.del_event(writer, &event, &uid)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn batch_put<II, N>(&self, events: II) -> Result<usize>
    where
        II: IntoIterator<Item = N>,
        N: AsRef<Event>,
    {
        let mut writer = self.inner.writer()?;
        let mut events = events.into_iter().collect::<Vec<N>>();

        // sort for check dup
        events.sort_by(|a, b| a.as_ref().id().cmp(&b.as_ref().id()));
        let mut count = 0;

        for (i, event) in events.iter().enumerate() {
            let event = event.as_ref();
            // dup in the input events
            if i != 0 && event.id() == events[i - 1].as_ref().id() {
                continue;
            }
            if let CheckEventResult::Ok(c) = self.put(&mut writer, event)? {
                count += c;
            }
        }

        writer.commit()?;
        Ok(count)
    }

    pub fn batch_get<R: FromEventData, II, N>(&self, event_ids: II) -> Result<Vec<R>>
    where
        II: IntoIterator<Item = N>,
        N: AsRef<[u8]>,
    {
        let reader = self.reader()?;
        let mut events = vec![];
        for id in event_ids.into_iter() {
            let r = self.get::<R, _, _>(&reader, &id)?;
            if let Some(e) = r {
                events.push(e);
            }
        }
        Ok(events)
    }

    pub fn batch_del<II, N>(&self, event_ids: II) -> Result<()>
    where
        II: IntoIterator<Item = N>,
        N: AsRef<[u8]>,
    {
        let mut writer = self.inner.writer()?;
        for id in event_ids.into_iter() {
            self.del(&mut writer, &id)?;
        }
        writer.commit()?;
        Ok(())
    }

    pub fn iter<'txn, J: FromEventData, T: Transaction>(
        &self,
        txn: &'txn T,
        filter: &Filter,
    ) -> Result<Iter<'txn, T, J>> {
        if let Some(_) = filter.search.as_ref() {
            let match_index = if filter.ids.is_some()
                || filter.tags.len() > 0
                || filter.authors.is_some()
                || filter.kinds.is_some()
            {
                MatchIndex::All
            } else {
                MatchIndex::None
            };
            Iter::new_word(self, txn, filter, &self.t_word, match_index)
        } else if let Some(ids) = filter.ids.as_ref() {
            let match_index =
                if filter.tags.len() > 0 || filter.authors.is_some() || filter.kinds.is_some() {
                    MatchIndex::All
                } else {
                    MatchIndex::None
                };
            Iter::new_prefix(self, txn, filter, ids, &self.t_id, match_index)
        } else if filter.tags.len() > 0 {
            let match_index = if filter.authors.is_some() {
                MatchIndex::Pubkey
            } else {
                MatchIndex::None
            };
            Iter::new_tag(self, txn, filter, &self.t_tag, match_index)
        } else if filter.authors.is_some() && filter.kinds.is_some() {
            Iter::new_author_kind(self, txn, filter, &self.t_pubkey_kind, MatchIndex::None)
        } else if let Some(ids) = filter.authors.as_ref() {
            Iter::new_prefix(self, txn, filter, ids, &self.t_pubkey, MatchIndex::None)
        } else if filter.kinds.is_some() {
            Iter::new_kind(self, txn, filter, &self.t_kind, MatchIndex::None)
        } else {
            Iter::new_time(self, txn, filter, &self.t_created_at, MatchIndex::None)
        }
    }

    pub fn iter_expiration<'txn, J: FromEventData, T: Transaction>(
        &self,
        txn: &'txn T,
        until: Option<u64>,
    ) -> Result<Iter<'txn, T, J>> {
        let filter = Filter {
            desc: true,
            until,
            ..Default::default()
        };
        Iter::new_time(self, txn, &filter, &self.t_expiration, MatchIndex::None)
    }
}

// type IterChecker<I, E> =
//     Box<dyn Fn(&Scanner<I, IndexKey>, &IndexKey) -> Result<CheckResult, Error<E>>>;
// #[allow(unused)]
// enum CheckResult {
//     Continue,
//     Found,
// }

#[derive(Debug)]
enum MatchIndex {
    All,
    Pubkey,
    None,
}

impl MatchIndex {
    fn r#match(&self, filter: &Filter, event: &ArchivedEventIndex) -> bool {
        match &self {
            MatchIndex::Pubkey => {
                Filter::match_author(filter.authors.as_ref(), event.pubkey(), event.delegator())
            }
            _ => filter.match_archived(event),
        }
    }
}

pub struct Iter<'txn, R, J>
where
    R: Transaction,
{
    reader: &'txn R,
    view_data: Tree,
    view_index: Tree,
    group: Group<'txn, IndexKey, Error>,
    get_data: u64,
    get_index: u64,
    filter: Filter,
    // checker: Option<IterChecker<D::Iter, D::Error>>,
    _r: PhantomData<J>,
    // need get index data for filter
    match_index: MatchIndex,
}

fn create_iter<'a, R: Transaction>(
    reader: &'a R,
    tree: &Tree,
    prefix: &Vec<u8>,
    reverse: bool,
) -> LmdbIter<'a> {
    if reverse {
        let start = upper(prefix.clone())
            .map(|p| Bound::Excluded(p))
            .unwrap_or(Bound::Unbounded);
        reader.iter_from(tree, start, true)
    } else {
        reader.iter_from(tree, Bound::Included(&prefix), false)
    }
}

impl<'txn, R, J> Iter<'txn, R, J>
where
    R: Transaction,
    J: FromEventData,
{
    fn new(
        kv_db: &Db,
        reader: &'txn R,
        filter: &Filter,
        group: Group<'txn, IndexKey, Error>,
        match_index: MatchIndex,
    ) -> Result<Self, Error> {
        Ok(Self {
            view_data: kv_db.t_data.clone(),
            view_index: kv_db.t_index.clone(),
            reader,
            group,
            get_data: 0,
            get_index: 0,
            filter: filter.clone(),
            // checker: None,
            _r: PhantomData,
            match_index,
        })
    }

    /// Filter from timestamp index
    fn new_time(
        kv_db: &Db,
        reader: &'txn R,
        filter: &Filter,
        view: &Tree,
        match_index: MatchIndex,
    ) -> Result<Self, Error> {
        let mut group = Group::new(filter.desc, false, false);
        let prefix = if filter.desc {
            (u64::MAX - 1).to_be_bytes()
        } else {
            0u64.to_be_bytes()
        }
        .to_vec();
        let iter = create_iter(reader, view, &prefix, filter.desc);
        let scanner = Scanner::new(
            iter,
            vec![],
            prefix.clone(),
            filter.desc,
            filter.since,
            filter.until,
            Box::new(|_, r| Ok(MatchResult::Found(IndexKey::from(r.0, r.1)?))),
        );
        group.add(scanner)?;
        Self::new(kv_db, reader, filter, group, match_index)
    }

    fn new_kind(
        kv_db: &Db,
        reader: &'txn R,
        filter: &Filter,
        view: &Tree,
        match_index: MatchIndex,
    ) -> Result<Self, Error> {
        let mut group = Group::new(filter.desc, false, false);
        for kind in filter.kinds.as_ref().unwrap().iter() {
            let prefix = u64_to_ver(*kind);
            let iter = create_iter(reader, view, &prefix, filter.desc);
            let scanner = Scanner::new(
                iter,
                vec![],
                prefix.clone(),
                filter.desc,
                filter.since,
                filter.until,
                Box::new(|s, r| {
                    let k = r.0;
                    Ok(if k.starts_with(&s.prefix) {
                        MatchResult::Found(IndexKey::from(k, r.1)?)
                    } else {
                        MatchResult::Stop
                    })
                }),
            );
            group.add(scanner)?;
        }
        Self::new(kv_db, reader, filter, group, match_index)
    }

    fn new_tag(
        kv_db: &Db,
        reader: &'txn R,
        filter: &Filter,
        view: &Tree,
        match_index: MatchIndex,
    ) -> Result<Self, Error> {
        let mut group = Group::new(filter.desc, false, true);
        let has_kind = filter.kinds.is_some();

        for tag in filter.tags.iter() {
            for key in tag.1.iter() {
                let kinds = filter.kinds.clone();
                // need add separator to the end, otherwise other tags will intrude
                // ["t", "nostr"]
                // ["t", "nostr1"]
                let prefix = concat_sep(concat_sep(tag.0, key), vec![]);
                let klen = prefix.len() + 8;
                let iter = create_iter(reader, view, &prefix, filter.desc);

                let scanner = Scanner::new(
                    iter,
                    vec![],
                    prefix.clone(),
                    filter.desc,
                    filter.since,
                    filter.until,
                    Box::new(move |s, r| {
                        let k = r.0;
                        let v = r.1;
                        Ok(if k.len() == klen && k.starts_with(&s.prefix) {
                            // filter
                            if has_kind
                                && !Filter::match_kind(kinds.as_ref(), u64_from_bytes(&v[8..16])?)
                            {
                                MatchResult::Continue
                            } else {
                                MatchResult::Found(IndexKey::from(k, v)?)
                            }
                        } else {
                            MatchResult::Stop
                        })
                    }),
                );
                group.add(scanner)?;
            }
        }
        Self::new(kv_db, reader, filter, group, match_index)
    }

    fn new_author_kind(
        kv_db: &Db,
        reader: &'txn R,
        filter: &Filter,
        view: &Tree,
        match_index: MatchIndex,
    ) -> Result<Self, Error> {
        let mut group = Group::new(filter.desc, false, false);
        let authors = filter.authors.as_ref().unwrap();
        let kinds = filter.kinds.as_ref().unwrap();
        let key_len = 32;

        for key in authors.iter() {
            let odd = key.len() % 2 == 1;
            let prefix = if odd {
                // range 0 to f
                if filter.desc {
                    hex::decode(key.to_string() + "f")
                } else {
                    hex::decode(key.to_string() + "0")
                }
            } else {
                hex::decode(key)
            };
            let prefix = prefix?;
            // full key
            if key.len() == key_len * 2 {
                for kind in kinds.iter() {
                    let prefix: Vec<u8> = concat(&prefix, u64_to_ver(*kind));
                    let iter = create_iter(reader, view, &prefix, filter.desc);
                    let scanner = Scanner::new(
                        iter,
                        key.as_bytes().to_vec(),
                        prefix.clone(),
                        filter.desc,
                        filter.since,
                        filter.until,
                        Box::new(|s, r| {
                            let k = r.0;
                            Ok(if k.starts_with(&s.prefix) {
                                MatchResult::Found(IndexKey::from(k, r.1)?)
                            } else {
                                MatchResult::Stop
                            })
                        }),
                    );
                    group.add(scanner)?;
                }
            } else {
                let clone_kinds = kinds.clone();
                // like scan by author, check kind later
                let iter = create_iter(reader, view, &prefix, filter.desc);

                let scanner = Scanner::new(
                    iter,
                    key.as_bytes().to_vec(),
                    prefix.clone(),
                    filter.desc,
                    filter.since,
                    filter.until,
                    Box::new(move |s, r| {
                        let k = r.0;
                        let ok = if odd {
                            hex::encode(k).as_bytes().starts_with(&s.key)
                        } else {
                            k.starts_with(&s.prefix)
                        };
                        Ok(if ok {
                            // check kind
                            let kind = u64_from_bytes(&k[32..40])?;
                            if !clone_kinds.contains(&kind) {
                                MatchResult::Continue
                            } else {
                                MatchResult::Found(IndexKey::from(k, r.1)?)
                            }
                        } else {
                            MatchResult::Stop
                        })
                    }),
                );
                group.add(scanner)?;
            }
        }

        Self::new(kv_db, reader, filter, group, match_index)
    }

    fn new_prefix(
        kv_db: &Db,
        reader: &'txn R,
        filter: &Filter,
        ids: &Vec<String>,
        view: &Tree,
        match_index: MatchIndex,
    ) -> Result<Self, Error> {
        let mut group = Group::new(filter.desc, false, false);

        for key in ids.iter() {
            let odd = key.len() % 2 == 1;
            let prefix = if odd {
                // range 0 to f
                if filter.desc {
                    hex::decode(key.to_string() + "f")
                } else {
                    hex::decode(key.to_string() + "0")
                }
            } else {
                hex::decode(key)
            };
            let prefix = prefix?;
            let iter = create_iter(reader, view, &prefix, filter.desc);
            let scanner = Scanner::new(
                iter,
                key.as_bytes().to_vec(),
                prefix.clone(),
                filter.desc,
                filter.since,
                filter.until,
                Box::new(move |s, r| {
                    let k = r.0.as_ref();
                    let ok = if odd {
                        hex::encode(k).as_bytes().starts_with(&s.key)
                    } else {
                        k.starts_with(&s.prefix)
                    };
                    Ok(if ok {
                        MatchResult::Found(IndexKey::from(k, r.1)?)
                    } else {
                        MatchResult::Stop
                    })
                }),
            );
            group.add(scanner)?;
        }
        Self::new(kv_db, reader, filter, group, match_index)
    }

    fn new_word(
        kv_db: &Db,
        reader: &'txn R,
        filter: &Filter,
        view: &Tree,
        match_index: MatchIndex,
    ) -> Result<Self, Error> {
        let mut group = Group::new(filter.desc, true, true);
        if let Some(words) = &filter.words {
            for word in words {
                let prefix = concat_sep(word, []);
                let klen = prefix.len() + 8;
                let iter = create_iter(reader, view, &prefix, filter.desc);
                let scanner = Scanner::new(
                    iter,
                    vec![],
                    prefix.clone(),
                    filter.desc,
                    filter.since,
                    filter.until,
                    Box::new(move |s, r| {
                        let k = r.0;
                        Ok(if k.len() == klen && k.starts_with(&s.prefix) {
                            MatchResult::Found(IndexKey::from(k, r.1)?)
                        } else {
                            MatchResult::Stop
                        })
                    }),
                );
                group.add(scanner)?;
            }
        }
        Self::new(kv_db, reader, filter, group, match_index)
    }

    fn document(&self, key: &IndexKey) -> Result<Option<J>, Error> {
        get_event_by_uid::<J, _, _>(self.reader, &self.view_data, key.uid().to_be_bytes())
    }

    fn index_data(&self, key: &IndexKey) -> Result<Option<&'txn [u8]>, Error> {
        let v = self.reader.get(&self.view_index, key.uid().to_be_bytes())?;
        Ok(v)
    }

    fn decode_event<'a>(
        &self,
        v: &'a Option<&[u8]>,
    ) -> Result<Option<&'a ArchivedEventIndex>, Error> {
        if let Some(v) = v {
            return Ok(Some(EventIndex::from_zeroes(v.as_ref())?));
        }
        return Ok(None);
    }

    fn limit(&self, num: u64) -> bool {
        if let Some(limit) = self.filter.limit {
            num >= limit
        } else {
            false
        }
    }

    fn next_inner(&mut self) -> Result<Option<J>, Error> {
        while let Some(item) = self.group.next() {
            let key = item?;
            if matches!(self.match_index, MatchIndex::None) {
                self.get_data += 1;
                if let Some(event) = self.document(&key)? {
                    return Ok(Some(event));
                }
            } else {
                let data = self.index_data(&key)?;
                let event = self.decode_event(&data)?;
                self.get_index += 1;
                if let Some(event) = event {
                    if self.match_index.r#match(&self.filter, event) {
                        self.get_data += 1;
                        if let Some(event) = self.document(&key)? {
                            return Ok(Some(event));
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

impl<'txn, R, J> Iter<'txn, R, J>
where
    R: Transaction,
    J: FromEventData,
{
    pub fn stats(&self) -> Stats {
        Stats {
            scan_index: self.group.scan_index,
            get_data: self.get_data,
            get_index: self.get_index,
        }
    }

    pub fn size(mut self) -> Result<(u64, Stats)> {
        let mut len = 0;
        while let Some(item) = self.group.next() {
            let key = item?;
            if matches!(self.match_index, MatchIndex::None) {
                len += 1;
                if self.limit(len) {
                    break;
                }
            } else {
                let data = self.index_data(&key)?;
                let event = self.decode_event(&data)?;
                self.get_index += 1;
                if let Some(event) = event {
                    if self.match_index.r#match(&self.filter, event) {
                        len += 1;
                        if self.limit(len) {
                            break;
                        }
                    }
                }
            }
        }
        Ok((
            len,
            Stats {
                get_data: 0,
                get_index: self.get_index,
                scan_index: self.group.scan_index,
            },
        ))
    }
}

impl<'txn, R, J> Iterator for Iter<'txn, R, J>
where
    R: Transaction,
    J: FromEventData,
{
    type Item = Result<J, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.limit(self.get_data) {
            None
        } else {
            self.next_inner().transpose()
        }
    }
}
