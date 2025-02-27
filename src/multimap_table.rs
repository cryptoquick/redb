use crate::multimap_table::DynamicCollectionType::{Inline, Subtree};
use crate::tree_store::{
    AllPageNumbersBtreeIter, Btree, BtreeMut, BtreeRangeIter, Checksum, LeafAccessor, Page,
    PageHint, PageNumber, RawLeafBuilder, TransactionalMemory, BRANCH, LEAF,
};
use crate::types::{RedbKey, RedbValue, TypeName};
use crate::{AccessGuard, Result, WriteTransaction};
use std::borrow::Borrow;
use std::convert::TryInto;
use std::marker::PhantomData;
use std::mem;
use std::mem::size_of;
use std::ops::{RangeBounds, RangeFull};
use std::sync::{Arc, Mutex};

pub(crate) fn parse_subtree_roots<T: Page>(
    page: &T,
    fixed_key_size: Option<usize>,
    fixed_value_size: Option<usize>,
) -> Vec<PageNumber> {
    match page.memory()[0] {
        BRANCH => {
            vec![]
        }
        LEAF => {
            let mut result = vec![];
            let accessor = LeafAccessor::new(page.memory(), fixed_key_size, fixed_value_size);
            for i in 0..accessor.num_pairs() {
                let entry = accessor.entry(i).unwrap();
                let collection = <&DynamicCollection>::from_bytes(entry.value());
                if matches!(collection.collection_type(), DynamicCollectionType::Subtree) {
                    result.push(collection.as_subtree().0);
                }
            }

            result
        }
        _ => unreachable!(),
    }
}

pub(crate) struct LeafKeyIter<'a> {
    inline_collection: AccessGuard<'a, &'static DynamicCollection>,
    fixed_key_size: Option<usize>,
    fixed_value_size: Option<usize>,
    start_entry: isize, // inclusive
    end_entry: isize,   // inclusive
}

impl<'a> LeafKeyIter<'a> {
    fn new(
        data: AccessGuard<'a, &'static DynamicCollection>,
        fixed_key_size: Option<usize>,
        fixed_value_size: Option<usize>,
    ) -> Self {
        let accessor =
            LeafAccessor::new(data.value().as_inline(), fixed_key_size, fixed_value_size);
        let end_entry = isize::try_from(accessor.num_pairs()).unwrap() - 1;
        Self {
            inline_collection: data,
            fixed_key_size,
            fixed_value_size,
            start_entry: 0,
            end_entry,
        }
    }

    fn next_key(&mut self) -> Option<&[u8]> {
        if self.end_entry < self.start_entry {
            return None;
        }
        let accessor = LeafAccessor::new(
            self.inline_collection.value().as_inline(),
            self.fixed_key_size,
            self.fixed_value_size,
        );
        self.start_entry += 1;
        accessor
            .entry((self.start_entry - 1).try_into().unwrap())
            .map(|e| e.key())
    }

    fn next_key_back(&mut self) -> Option<&[u8]> {
        if self.end_entry < self.start_entry {
            return None;
        }
        let accessor = LeafAccessor::new(
            self.inline_collection.value().as_inline(),
            self.fixed_key_size,
            self.fixed_value_size,
        );
        self.end_entry -= 1;
        accessor
            .entry((self.end_entry + 1).try_into().unwrap())
            .map(|e| e.key())
    }
}

enum DynamicCollectionType {
    Inline,
    Subtree,
}

impl From<u8> for DynamicCollectionType {
    fn from(value: u8) -> Self {
        match value {
            LEAF => Inline,
            2 => Subtree,
            _ => unreachable!(),
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<u8> for DynamicCollectionType {
    fn into(self) -> u8 {
        match self {
            // Reuse the LEAF type id, so that we can cast this directly into the format used by
            // LeafAccessor
            Inline => LEAF,
            Subtree => 2,
        }
    }
}

/// Layout:
/// type (1 byte):
/// * 1 = inline data
/// * 2 = sub tree
///
/// (when type = 1) data (n bytes): inlined leaf node
///
/// (when type = 2) root (8 bytes): sub tree root page number
/// (when type = 2) checksum (16 bytes): sub tree checksum
#[derive(Debug)]
#[repr(transparent)]
struct DynamicCollection {
    data: [u8],
    // TODO: include V type when GATs are stable
    // _value_type: PhantomData<V>,
}

impl RedbValue for &DynamicCollection {
    type SelfType<'a> = &'a DynamicCollection
    where
        Self: 'a;
    type AsBytes<'a> = &'a [u8]
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> &'a DynamicCollection
    where
        Self: 'a,
    {
        DynamicCollection::new(data)
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> &'a [u8]
    where
        Self: 'a,
        Self: 'b,
    {
        &value.data
    }

    fn type_name() -> TypeName {
        TypeName::internal("redb::DynamicCollection")
    }
}

impl DynamicCollection {
    fn new(data: &[u8]) -> &Self {
        unsafe { mem::transmute(data) }
    }

    fn collection_type(&self) -> DynamicCollectionType {
        DynamicCollectionType::from(self.data[0])
    }

    fn as_inline(&self) -> &[u8] {
        debug_assert!(matches!(self.collection_type(), Inline));
        &self.data[1..]
    }

    fn as_subtree(&self) -> (PageNumber, Checksum) {
        debug_assert!(matches!(self.collection_type(), Subtree));
        let offset = 1 + PageNumber::serialized_size();
        let page_number = PageNumber::from_le_bytes(self.data[1..offset].try_into().unwrap());
        let checksum = Checksum::from_le_bytes(
            self.data[offset..(offset + size_of::<Checksum>())]
                .try_into()
                .unwrap(),
        );
        (page_number, checksum)
    }

    fn iter<'a, V: RedbKey>(
        collection: AccessGuard<'a, &'static DynamicCollection>,
        mem: &'a TransactionalMemory,
    ) -> Result<MultimapValueIter<'a, V>> {
        Ok(match collection.value().collection_type() {
            Inline => {
                let leaf_iter = LeafKeyIter::new(
                    collection,
                    V::fixed_width(),
                    <() as RedbValue>::fixed_width(),
                );
                MultimapValueIter::new_inline(leaf_iter)
            }
            Subtree => {
                let root = collection.value().as_subtree().0;
                MultimapValueIter::new_subtree(BtreeRangeIter::new::<RangeFull, &V::SelfType<'_>>(
                    ..,
                    Some(root),
                    mem,
                )?)
            }
        })
    }

    fn iter_free_on_drop<'a, V: RedbKey>(
        collection: AccessGuard<'a, &'static DynamicCollection>,
        pages: Vec<PageNumber>,
        freed_pages: Arc<Mutex<Vec<PageNumber>>>,
        mem: &'a TransactionalMemory,
    ) -> Result<MultimapValueIter<'a, V>> {
        Ok(match collection.value().collection_type() {
            Inline => {
                let leaf_iter = LeafKeyIter::new(
                    collection,
                    V::fixed_width(),
                    <() as RedbValue>::fixed_width(),
                );
                MultimapValueIter::new_inline(leaf_iter)
            }
            Subtree => {
                let root = collection.value().as_subtree().0;
                let inner =
                    BtreeRangeIter::new::<RangeFull, &V::SelfType<'_>>(.., Some(root), mem)?;
                MultimapValueIter::new_subtree_free_on_drop(inner, freed_pages, pages, mem)
            }
        })
    }

    fn make_inline_data(data: &[u8]) -> Vec<u8> {
        let mut result = vec![Inline.into()];
        result.extend_from_slice(data);

        result
    }

    fn make_subtree_data(root: PageNumber, checksum: Checksum) -> Vec<u8> {
        let mut result = vec![Subtree.into()];
        result.extend_from_slice(&root.to_le_bytes());
        result.extend_from_slice(Checksum::as_bytes(&checksum).as_ref());

        result
    }
}

enum ValueIterState<'a, V: RedbKey + 'static> {
    Subtree(BtreeRangeIter<'a, V, ()>),
    InlineLeaf(LeafKeyIter<'a>),
}

pub struct MultimapValueIter<'a, V: RedbKey + 'static> {
    inner: Option<ValueIterState<'a, V>>,
    freed_pages: Option<Arc<Mutex<Vec<PageNumber>>>>,
    free_on_drop: Vec<PageNumber>,
    mem: Option<&'a TransactionalMemory>,
    _value_type: PhantomData<V>,
}

impl<'a, V: RedbKey + 'static> MultimapValueIter<'a, V> {
    fn new_subtree(inner: BtreeRangeIter<'a, V, ()>) -> Self {
        Self {
            inner: Some(ValueIterState::Subtree(inner)),
            freed_pages: None,
            free_on_drop: vec![],
            mem: None,
            _value_type: Default::default(),
        }
    }

    fn new_subtree_free_on_drop(
        inner: BtreeRangeIter<'a, V, ()>,
        freed_pages: Arc<Mutex<Vec<PageNumber>>>,
        pages: Vec<PageNumber>,
        mem: &'a TransactionalMemory,
    ) -> Self {
        Self {
            inner: Some(ValueIterState::Subtree(inner)),
            freed_pages: Some(freed_pages),
            free_on_drop: pages,
            mem: Some(mem),
            _value_type: Default::default(),
        }
    }

    fn new_inline(inner: LeafKeyIter<'a>) -> Self {
        Self {
            inner: Some(ValueIterState::InlineLeaf(inner)),
            freed_pages: None,
            free_on_drop: vec![],
            mem: None,
            _value_type: Default::default(),
        }
    }
}

impl<'a, V: RedbKey + 'static> Iterator for MultimapValueIter<'a, V> {
    type Item = AccessGuard<'a, V>;

    fn next(&mut self) -> Option<Self::Item> {
        // TODO: optimize out this copy
        let bytes = match self.inner.as_mut().unwrap() {
            ValueIterState::Subtree(ref mut iter) => iter.next().map(|e| e.key_data())?,
            ValueIterState::InlineLeaf(ref mut iter) => iter.next_key()?.to_vec(),
        };
        Some(AccessGuard::with_owned_value(bytes))
    }
}

impl<'a, V: RedbKey + 'static> DoubleEndedIterator for MultimapValueIter<'a, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        // TODO: optimize out this copy
        let bytes = match self.inner.as_mut().unwrap() {
            ValueIterState::Subtree(ref mut iter) => iter.next_back().map(|e| e.key_data())?,
            ValueIterState::InlineLeaf(ref mut iter) => iter.next_key_back()?.to_vec(),
        };
        Some(AccessGuard::with_owned_value(bytes))
    }
}

impl<'a, V: RedbKey + 'static> Drop for MultimapValueIter<'a, V> {
    fn drop(&mut self) {
        // Drop our references to the pages that are about to be freed
        drop(mem::take(&mut self.inner));
        if !self.free_on_drop.is_empty() {
            let mut freed_pages = self.freed_pages.as_ref().unwrap().lock().unwrap();
            for page in self.free_on_drop.iter() {
                unsafe {
                    // Safety: we have a &mut on the transaction
                    if !self.mem.unwrap().free_if_uncommitted(*page) {
                        freed_pages.push(*page);
                    }
                }
            }
        }
    }
}

pub struct MultimapRangeIter<'a, K: RedbKey + 'static, V: RedbKey + 'static> {
    inner: BtreeRangeIter<'a, K, &'static DynamicCollection>,
    mem: &'a TransactionalMemory,
    _key_type: PhantomData<K>,
    _value_type: PhantomData<V>,
}

impl<'a, K: RedbKey + 'static, V: RedbKey + 'static> MultimapRangeIter<'a, K, V> {
    fn new(
        inner: BtreeRangeIter<'a, K, &'static DynamicCollection>,
        mem: &'a TransactionalMemory,
    ) -> Self {
        Self {
            inner,
            mem,
            _key_type: Default::default(),
            _value_type: Default::default(),
        }
    }
}

impl<'a, K: RedbKey + 'static, V: RedbKey + 'static> Iterator for MultimapRangeIter<'a, K, V> {
    type Item = (AccessGuard<'a, K>, MultimapValueIter<'a, V>);

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.inner.next()?;
        let key = AccessGuard::with_owned_value(entry.key_data());
        let (page, _, value_range) = entry.into_raw();
        let collection = AccessGuard::with_page(page, value_range);
        // TODO: propagate error
        let iter = DynamicCollection::iter(collection, self.mem).unwrap();

        Some((key, iter))
    }
}

impl<'a, K: RedbKey + 'static, V: RedbKey + 'static> DoubleEndedIterator
    for MultimapRangeIter<'a, K, V>
{
    fn next_back(&mut self) -> Option<Self::Item> {
        let entry = self.inner.next_back()?;
        let key = AccessGuard::with_owned_value(entry.key_data());
        let (page, _, value_range) = entry.into_raw();
        let collection = AccessGuard::with_page(page, value_range);
        // TODO: propagate error
        let iter = DynamicCollection::iter(collection, self.mem).unwrap();

        Some((key, iter))
    }
}

/// A multimap table
///
/// [Multimap tables](https://en.wikipedia.org/wiki/Multimap) may have multiple values associated with each key
pub struct MultimapTable<'db, 'txn, K: RedbKey + 'static, V: RedbKey + 'static> {
    name: String,
    transaction: &'txn WriteTransaction<'db>,
    freed_pages: Arc<Mutex<Vec<PageNumber>>>,
    tree: BtreeMut<'txn, K, &'static DynamicCollection>,
    mem: &'db TransactionalMemory,
    _value_type: PhantomData<V>,
}

impl<'db, 'txn, K: RedbKey + 'static, V: RedbKey + 'static> MultimapTable<'db, 'txn, K, V> {
    pub(crate) fn new(
        name: &str,
        table_root: Option<(PageNumber, Checksum)>,
        freed_pages: Arc<Mutex<Vec<PageNumber>>>,
        mem: &'db TransactionalMemory,
        transaction: &'txn WriteTransaction<'db>,
    ) -> MultimapTable<'db, 'txn, K, V> {
        MultimapTable {
            name: name.to_string(),
            transaction,
            freed_pages: freed_pages.clone(),
            tree: BtreeMut::new(table_root, mem, freed_pages),
            mem,
            _value_type: Default::default(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn print_debug(&self, include_values: bool) -> Result {
        self.tree.print_debug(include_values)
    }

    /// Add the given value to the mapping of the key
    ///
    /// Returns `true` if the key-value pair was present
    pub fn insert<'a>(
        &mut self,
        key: impl Borrow<K::SelfType<'a>>,
        value: impl Borrow<V::SelfType<'a>>,
    ) -> Result<bool>
    where
        K: 'a,
        V: 'a,
    {
        let value_bytes = V::as_bytes(value.borrow());
        let value_bytes_ref = value_bytes.as_ref();
        let get_result = self.tree.get(key.borrow())?;
        let existed = if get_result.is_some() {
            #[allow(clippy::unnecessary_unwrap)]
            let guard = get_result.unwrap();
            let collection_type = guard.value().collection_type();
            match collection_type {
                Inline => {
                    let leaf_data = guard.value().as_inline();
                    let accessor = LeafAccessor::new(
                        leaf_data,
                        V::fixed_width(),
                        <() as RedbValue>::fixed_width(),
                    );
                    let (position, found) = accessor.position::<V>(value_bytes_ref);
                    if found {
                        return Ok(true);
                    }

                    let new_pairs = accessor.num_pairs() + 1;
                    let new_pair_bytes =
                        accessor.length_of_pairs(0, accessor.num_pairs()) + value_bytes_ref.len();
                    let new_key_bytes =
                        accessor.length_of_keys(0, accessor.num_pairs()) + value_bytes_ref.len();
                    let required_inline_bytes =
                        RawLeafBuilder::required_bytes(new_pairs, new_pair_bytes);

                    if required_inline_bytes < self.mem.get_page_size() / 2 {
                        let mut data = vec![0; required_inline_bytes];
                        let mut builder = RawLeafBuilder::new(
                            &mut data,
                            new_pairs,
                            V::fixed_width(),
                            <() as RedbValue>::fixed_width(),
                            new_key_bytes,
                        );
                        for i in 0..accessor.num_pairs() {
                            if i == position {
                                builder.append(
                                    value_bytes_ref,
                                    <() as RedbValue>::as_bytes(&()).as_ref(),
                                );
                            }
                            let entry = accessor.entry(i).unwrap();
                            builder.append(entry.key(), entry.value());
                        }
                        if position == accessor.num_pairs() {
                            builder
                                .append(value_bytes_ref, <() as RedbValue>::as_bytes(&()).as_ref());
                        }
                        drop(builder);
                        drop(guard);
                        let inline_data = DynamicCollection::make_inline_data(&data);
                        unsafe {
                            self.tree
                                .insert(key.borrow(), &DynamicCollection::new(&inline_data))?
                        };
                    } else {
                        // convert into a subtree
                        let mut page = self.mem.allocate(leaf_data.len())?;
                        page.memory_mut()[..leaf_data.len()].copy_from_slice(leaf_data);
                        let page_number = page.get_page_number();
                        drop(page);
                        drop(guard);

                        // Don't bother computing the checksum, since we're about to modify the tree
                        let mut subtree: BtreeMut<'_, V, ()> = BtreeMut::new(
                            Some((page_number, 0)),
                            self.mem,
                            self.freed_pages.clone(),
                        );
                        // Safety: No other references to this table can exist.
                        // Tables can only be opened mutably in one location (see Error::TableAlreadyOpen),
                        // and we borrow &mut self.
                        let existed = unsafe { subtree.insert(value.borrow(), &())?.is_some() };
                        assert_eq!(existed, found);
                        let (new_root, new_checksum) = subtree.get_root().unwrap();
                        let subtree_data =
                            DynamicCollection::make_subtree_data(new_root, new_checksum);
                        unsafe {
                            self.tree
                                .insert(key.borrow(), &DynamicCollection::new(&subtree_data))?
                        };
                    }

                    found
                }
                Subtree => {
                    let mut subtree: BtreeMut<'_, V, ()> = BtreeMut::new(
                        Some(guard.value().as_subtree()),
                        self.mem,
                        self.freed_pages.clone(),
                    );
                    drop(guard);
                    // Safety: No other references to this table can exist.
                    // Tables can only be opened mutably in one location (see Error::TableAlreadyOpen),
                    // and we borrow &mut self.
                    let existed = unsafe { subtree.insert(value.borrow(), &())?.is_some() };
                    let (new_root, new_checksum) = subtree.get_root().unwrap();
                    let subtree_data = DynamicCollection::make_subtree_data(new_root, new_checksum);
                    unsafe {
                        self.tree
                            .insert(key.borrow(), &DynamicCollection::new(&subtree_data))?
                    };

                    existed
                }
            }
        } else {
            drop(get_result);
            let required_inline_bytes = RawLeafBuilder::required_bytes(1, value_bytes_ref.len());
            if required_inline_bytes < self.mem.get_page_size() / 2 {
                let mut data = vec![0; required_inline_bytes];
                let mut builder = RawLeafBuilder::new(
                    &mut data,
                    1,
                    V::fixed_width(),
                    <() as RedbValue>::fixed_width(),
                    value_bytes_ref.len(),
                );
                builder.append(value_bytes_ref, <() as RedbValue>::as_bytes(&()).as_ref());
                drop(builder);
                let inline_data = DynamicCollection::make_inline_data(&data);
                unsafe {
                    self.tree
                        .insert(key.borrow(), &DynamicCollection::new(&inline_data))?
                };
            } else {
                let mut subtree: BtreeMut<'_, V, ()> =
                    BtreeMut::new(None, self.mem, self.freed_pages.clone());
                // Safety: No other references to this table can exist.
                // Tables can only be opened mutably in one location (see Error::TableAlreadyOpen),
                // and we borrow &mut self.
                unsafe { subtree.insert(value.borrow(), &())? };
                let (new_root, new_checksum) = subtree.get_root().unwrap();
                let subtree_data = DynamicCollection::make_subtree_data(new_root, new_checksum);
                unsafe {
                    self.tree
                        .insert(key.borrow(), &DynamicCollection::new(&subtree_data))?
                };
            }
            false
        };

        Ok(existed)
    }

    /// Removes the given key-value pair
    ///
    /// Returns `true` if the key-value pair was present
    pub fn remove<'a>(
        &mut self,
        key: impl Borrow<K::SelfType<'a>>,
        value: impl Borrow<V::SelfType<'a>>,
    ) -> Result<bool>
    where
        K: 'a,
        V: 'a,
    {
        let get_result = self.tree.get(key.borrow())?;
        if get_result.is_none() {
            return Ok(false);
        }
        let guard = get_result.unwrap();
        let v = guard.value();
        let existed = match v.collection_type() {
            Inline => {
                let leaf_data = v.as_inline();
                let accessor = LeafAccessor::new(
                    leaf_data,
                    V::fixed_width(),
                    <() as RedbValue>::fixed_width(),
                );
                if let Some(position) = accessor.find_key::<V>(V::as_bytes(value.borrow()).as_ref())
                {
                    let old_num_pairs = accessor.num_pairs();
                    if old_num_pairs == 1 {
                        drop(guard);
                        unsafe { self.tree.remove(key.borrow())? };
                    } else {
                        let old_pairs_len = accessor.length_of_pairs(0, old_num_pairs);
                        let removed_value_len = accessor.entry(position).unwrap().key().len();
                        let required = RawLeafBuilder::required_bytes(
                            old_num_pairs - 1,
                            old_pairs_len - removed_value_len,
                        );
                        let mut new_data = vec![0; required];
                        let new_key_len =
                            accessor.length_of_keys(0, old_num_pairs) - removed_value_len;
                        let mut builder = RawLeafBuilder::new(
                            &mut new_data,
                            old_num_pairs - 1,
                            V::fixed_width(),
                            <() as RedbValue>::fixed_width(),
                            new_key_len,
                        );
                        for i in 0..old_num_pairs {
                            if i != position {
                                let entry = accessor.entry(i).unwrap();
                                builder.append(entry.key(), entry.value());
                            }
                        }
                        drop(builder);
                        drop(guard);

                        let inline_data = DynamicCollection::make_inline_data(&new_data);
                        unsafe {
                            self.tree
                                .insert(key.borrow(), &DynamicCollection::new(&inline_data))?
                        };
                    }
                    true
                } else {
                    drop(guard);
                    false
                }
            }
            Subtree => {
                let mut subtree: BtreeMut<V, ()> =
                    BtreeMut::new(Some(v.as_subtree()), self.mem, self.freed_pages.clone());
                drop(guard);
                // Safety: No other references to this table can exist.
                // Tables can only be opened mutably in one location (see Error::TableAlreadyOpen),
                // and we borrow &mut self.
                let existed = unsafe { subtree.remove(value.borrow())?.is_some() };

                if let Some((new_root, new_checksum)) = subtree.get_root() {
                    let page = self.mem.get_page(new_root)?;
                    match page.memory()[0] {
                        LEAF => {
                            let accessor = LeafAccessor::new(
                                page.memory(),
                                V::fixed_width(),
                                <() as RedbValue>::fixed_width(),
                            );
                            let len = accessor.total_length();
                            if len < self.mem.get_page_size() / 2 {
                                let inline_data =
                                    DynamicCollection::make_inline_data(&page.memory()[..len]);
                                unsafe {
                                    self.tree.insert(
                                        key.borrow(),
                                        &DynamicCollection::new(&inline_data),
                                    )?
                                };
                                drop(page);
                                unsafe {
                                    if !self.mem.free_if_uncommitted(new_root) {
                                        (*self.freed_pages).lock().unwrap().push(new_root);
                                    }
                                }
                            } else {
                                let subtree_data =
                                    DynamicCollection::make_subtree_data(new_root, new_checksum);
                                unsafe {
                                    self.tree.insert(
                                        key.borrow(),
                                        &DynamicCollection::new(&subtree_data),
                                    )?
                                };
                            }
                        }
                        BRANCH => {
                            unsafe {
                                let subtree_data =
                                    DynamicCollection::make_subtree_data(new_root, new_checksum);
                                self.tree
                                    .insert(key.borrow(), &DynamicCollection::new(&subtree_data))?
                            };
                        }
                        _ => unreachable!(),
                    }
                } else {
                    unsafe { self.tree.remove(key.borrow())? };
                }

                existed
            }
        };

        Ok(existed)
    }

    /// Removes all values for the given key
    ///
    /// Returns an iterator over the removed values. Values are in ascending order.
    pub fn remove_all<'a>(
        &mut self,
        key: impl Borrow<K::SelfType<'a>>,
    ) -> Result<MultimapValueIter<V>>
    where
        K: 'a,
    {
        // Safety: No other references to this table can exist.
        // Tables can only be opened mutably in one location (see Error::TableAlreadyOpen),
        // and we borrow &mut self.
        let iter = if let Some((collection, mut pages)) =
            self.tree.remove_retain_uncommitted(key.borrow())?
        {
            if matches!(
                collection.value().collection_type(),
                DynamicCollectionType::Subtree
            ) {
                let root = collection.value().as_subtree().0;
                let all_pages = AllPageNumbersBtreeIter::new(
                    root,
                    V::fixed_width(),
                    <() as RedbValue>::fixed_width(),
                    self.mem,
                )?;
                for page in all_pages {
                    pages.push(page);
                }
            }
            DynamicCollection::iter_free_on_drop(
                collection,
                pages,
                self.freed_pages.clone(),
                self.mem,
            )?
        } else {
            MultimapValueIter::new_subtree(BtreeRangeIter::new::<RangeFull, &V::SelfType<'_>>(
                ..,
                None,
                self.mem,
            )?)
        };

        Ok(iter)
    }
}

impl<'db, 'txn, K: RedbKey + 'static, V: RedbKey + 'static> ReadableMultimapTable<K, V>
    for MultimapTable<'db, 'txn, K, V>
{
    /// Returns an iterator over all values for the given key. Values are in ascending order.
    fn get<'a>(&'a self, key: impl Borrow<K::SelfType<'a>>) -> Result<MultimapValueIter<'a, V>> {
        let iter = if let Some(collection) = self.tree.get(key.borrow())? {
            DynamicCollection::iter(collection, self.mem)?
        } else {
            MultimapValueIter::new_subtree(BtreeRangeIter::new::<RangeFull, &V::SelfType<'_>>(
                ..,
                None,
                self.mem,
            )?)
        };

        Ok(iter)
    }

    /// Returns a double-ended iterator over a range of elements in the table
    fn range<'a: 'b, 'b, KR>(
        &'a self,
        range: impl RangeBounds<KR> + 'b,
    ) -> Result<MultimapRangeIter<'a, K, V>>
    where
        K: 'a,
        KR: Borrow<K::SelfType<'b>> + 'b,
    {
        let inner = self.tree.range(range)?;
        Ok(MultimapRangeIter::new(inner, self.mem))
    }

    /// Returns the number of key-value pairs in the table
    fn len(&self) -> Result<usize> {
        let mut count = 0;
        for (_, mut values) in self.iter()? {
            while values.next().is_some() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Returns `true` if the table is empty
    fn is_empty(&self) -> Result<bool> {
        self.len().map(|x| x == 0)
    }
}

impl<'db, 'txn, K: RedbKey + 'static, V: RedbKey + 'static> Drop
    for MultimapTable<'db, 'txn, K, V>
{
    fn drop(&mut self) {
        self.transaction.close_table(&self.name, &mut self.tree);
    }
}

pub trait ReadableMultimapTable<K: RedbKey + 'static, V: RedbKey + 'static> {
    /// Returns an iterator over all values for the given key. Values are in ascending order.
    fn get<'a>(&'a self, key: impl Borrow<K::SelfType<'a>>) -> Result<MultimapValueIter<'a, V>>
    where
        K: 'a;

    fn range<'a: 'b, 'b, KR>(
        &'a self,
        range: impl RangeBounds<KR> + 'b,
    ) -> Result<MultimapRangeIter<'a, K, V>>
    where
        K: 'a,
        KR: Borrow<K::SelfType<'b>> + 'b;

    fn len(&self) -> Result<usize>;

    fn is_empty(&self) -> Result<bool>;

    /// Returns an double-ended iterator over all elements in the table. Values are in ascending
    /// order.
    fn iter(&self) -> Result<MultimapRangeIter<K, V>> {
        self.range::<K::SelfType<'_>>(..)
    }
}

/// A read-only multimap table
pub struct ReadOnlyMultimapTable<'txn, K: RedbKey + 'static, V: RedbKey + 'static> {
    tree: Btree<'txn, K, &'static DynamicCollection>,
    mem: &'txn TransactionalMemory,
    _value_type: PhantomData<V>,
}

impl<'txn, K: RedbKey + 'static, V: RedbKey + 'static> ReadOnlyMultimapTable<'txn, K, V> {
    pub(crate) fn new(
        root_page: Option<(PageNumber, Checksum)>,
        hint: PageHint,
        mem: &'txn TransactionalMemory,
    ) -> ReadOnlyMultimapTable<'txn, K, V> {
        ReadOnlyMultimapTable {
            tree: Btree::new(root_page, hint, mem),
            mem,
            _value_type: Default::default(),
        }
    }
}

impl<'txn, K: RedbKey + 'static, V: RedbKey + 'static> ReadableMultimapTable<K, V>
    for ReadOnlyMultimapTable<'txn, K, V>
{
    /// Returns an iterator over all values for the given key. Values are in ascending order.
    fn get<'a>(&'a self, key: impl Borrow<K::SelfType<'a>>) -> Result<MultimapValueIter<'a, V>> {
        let iter = if let Some(collection) = self.tree.get(key.borrow())? {
            DynamicCollection::iter(collection, self.mem)?
        } else {
            MultimapValueIter::new_subtree(BtreeRangeIter::new::<RangeFull, &V::SelfType<'_>>(
                ..,
                None,
                self.mem,
            )?)
        };

        Ok(iter)
    }

    fn range<'a: 'b, 'b, KR>(
        &'a self,
        range: impl RangeBounds<KR> + 'b,
    ) -> Result<MultimapRangeIter<'a, K, V>>
    where
        K: 'a,
        KR: Borrow<K::SelfType<'b>> + 'b,
    {
        let inner = self.tree.range(range)?;
        Ok(MultimapRangeIter::new(inner, self.mem))
    }

    fn len(&self) -> Result<usize> {
        let mut count = 0;
        for (_, mut values) in self.iter()? {
            while values.next().is_some() {
                count += 1;
            }
        }
        Ok(count)
    }

    fn is_empty(&self) -> Result<bool> {
        self.len().map(|x| x == 0)
    }
}
