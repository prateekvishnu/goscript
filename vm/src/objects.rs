// Copyright 2022 The Goscript Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

#![macro_use]
use super::channel::Channel;
use super::ffi::Ffi;
use super::gc::GcoVec;
use super::instruction::{Instruction, OpIndex, Opcode, ValueType};
use super::metadata::*;
use super::stack::Stack;
use super::value::*;
use crate::value::GosElem;
use slotmap::{new_key_type, DenseSlotMap, KeyData};
use std::any::Any;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::convert::TryInto;
use std::fmt::Write;
use std::fmt::{self, Display};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Range;
use std::ptr::{self};
use std::rc::{Rc, Weak};
use std::{panic, str};

const DEFAULT_CAPACITY: usize = 128;

#[macro_export]
macro_rules! null_key {
    () => {
        slotmap::Key::null()
    };
}

new_key_type! { pub struct MetadataKey; }
new_key_type! { pub struct FunctionKey; }
new_key_type! { pub struct PackageKey; }

pub type MetadataObjs = DenseSlotMap<MetadataKey, MetadataType>;
pub type FunctionObjs = DenseSlotMap<FunctionKey, FunctionVal>;
pub type PackageObjs = DenseSlotMap<PackageKey, PackageVal>;

pub fn key_to_u64<K>(key: K) -> u64
where
    K: slotmap::Key,
{
    let data: slotmap::KeyData = key.data();
    data.as_ffi()
}

pub fn u64_to_key<K>(u: u64) -> K
where
    K: slotmap::Key,
{
    let data = slotmap::KeyData::from_ffi(u);
    data.into()
}

#[derive(Debug)]
pub struct VMObjects {
    pub metas: MetadataObjs,
    pub functions: FunctionObjs,
    pub packages: PackageObjs,
    pub s_meta: StaticMeta,
}

impl VMObjects {
    pub fn new() -> VMObjects {
        let mut metas = DenseSlotMap::with_capacity_and_key(DEFAULT_CAPACITY);
        let md = StaticMeta::new(&mut metas);
        VMObjects {
            metas: metas,
            functions: DenseSlotMap::with_capacity_and_key(DEFAULT_CAPACITY),
            packages: DenseSlotMap::with_capacity_and_key(DEFAULT_CAPACITY),
            s_meta: md,
        }
    }
}

// ----------------------------------------------------------------------------
// MapObj

pub type GosHashMap = HashMap<GosValue, GosValue>;

pub type GosHashMapIter<'a> = std::collections::hash_map::Iter<'a, GosValue, GosValue>;

#[derive(Debug)]
pub struct MapObj {
    zero_val: GosValue,
    map: RefCell<GosHashMap>,
}

impl MapObj {
    pub fn new(zero_val: GosValue) -> MapObj {
        MapObj {
            zero_val: zero_val,
            map: RefCell::new(HashMap::new()),
        }
    }

    #[inline]
    pub fn insert(&self, key: GosValue, val: GosValue) -> Option<GosValue> {
        self.borrow_data_mut().insert(key, val)
    }

    #[inline]
    pub fn new_default_val(&self, gcv: &GcoVec) -> GosValue {
        self.zero_val.copy_semantic(gcv)
    }

    #[inline]
    pub fn get(&self, key: &GosValue, gcv: &GcoVec) -> (GosValue, bool) {
        let mref = self.borrow_data();
        match mref.get(key) {
            Some(v) => (v.clone(), true),
            None => (self.new_default_val(gcv), false),
        }
    }

    #[inline]
    pub fn delete(&self, key: &GosValue) {
        let mut mref = self.borrow_data_mut();
        mref.remove(key);
    }

    /// touch_key makes sure there is a value for the 'key', a default value is set if
    /// the value is empty
    #[inline]
    pub fn touch_key(&self, key: &GosValue, gcv: &GcoVec) {
        if self.borrow_data().get(&key).is_none() {
            self.borrow_data_mut()
                .insert(key.clone(), self.new_default_val(gcv));
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.borrow_data().len()
    }

    #[inline]
    pub fn borrow_data_mut(&self) -> RefMut<GosHashMap> {
        self.map.borrow_mut()
    }

    #[inline]
    pub fn borrow_data(&self) -> Ref<GosHashMap> {
        self.map.borrow()
    }

    #[inline]
    pub fn clone_inner(&self) -> RefCell<GosHashMap> {
        self.map.clone()
    }
}

impl Clone for MapObj {
    fn clone(&self) -> Self {
        MapObj {
            zero_val: self.zero_val.clone(),
            map: self.map.clone(),
        }
    }
}

impl PartialEq for MapObj {
    fn eq(&self, _other: &MapObj) -> bool {
        unreachable!()
    }
}

impl Eq for MapObj {}

impl Display for MapObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("map[")?;
        for (i, kv) in self.map.borrow().iter().enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            let v: &GosValue = &kv.1;
            write!(f, "{}:{}", kv.0, v)?
        }
        f.write_char(']')
    }
}

// ----------------------------------------------------------------------------
// ArrayObj

pub struct ArrayObj<T> {
    vec: RefCell<Vec<T>>,
}

pub type GosArrayObj = ArrayObj<GosElem>;

impl<T> ArrayObj<T>
where
    T: Element,
{
    pub fn with_size(size: usize, cap: usize, val: &GosValue, gcos: &GcoVec) -> ArrayObj<T> {
        assert!(cap >= size);
        let mut v = Vec::with_capacity(cap);
        for _ in 0..size {
            v.push(T::from_value(val.copy_semantic(gcos)))
        }
        ArrayObj {
            vec: RefCell::new(v),
        }
    }

    pub fn with_data(data: Vec<GosValue>) -> ArrayObj<T> {
        ArrayObj {
            vec: RefCell::new(data.into_iter().map(|x| T::from_value(x)).collect()),
        }
    }

    pub fn with_raw_data(data: Vec<T>) -> ArrayObj<T> {
        ArrayObj {
            vec: RefCell::new(data),
        }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.borrow_data().len()
    }

    #[inline(always)]
    pub fn borrow_data_mut(&self) -> std::cell::RefMut<Vec<T>> {
        self.vec.borrow_mut()
    }

    #[inline(always)]
    pub fn borrow_data(&self) -> std::cell::Ref<Vec<T>> {
        self.vec.borrow()
    }

    #[inline]
    pub fn as_rust_slice(&self) -> Ref<[T]> {
        Ref::map(self.borrow_data(), |x| &x[..])
    }

    #[inline]
    pub fn as_rust_slice_mut(&self) -> RefMut<[T]> {
        RefMut::map(self.borrow_data_mut(), |x| &mut x[..])
    }

    #[inline(always)]
    pub fn index_elem(&self, i: usize) -> T {
        self.borrow_data()[i].clone()
    }

    #[inline(always)]
    pub fn get(&self, i: usize, t: ValueType) -> RuntimeResult<GosValue> {
        if i >= self.len() {
            return Err(format!("index {} out of range", i).to_owned());
        }
        Ok(self.borrow_data()[i].clone().into_value(t))
    }

    #[inline(always)]
    pub fn set(&self, i: usize, val: &GosValue) -> RuntimeResult<()> {
        if i >= self.len() {
            return Err(format!("index {} out of range", i).to_owned());
        }
        Ok(self.borrow_data()[i].set_value(&val))
    }

    #[inline]
    pub fn size_of_data(&self) -> usize {
        std::mem::size_of::<T>() * self.len()
    }

    pub fn display_fmt(&self, t: ValueType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('[')?;
        for (i, e) in self.vec.borrow().iter().enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            write!(f, "{}", e.clone().into_value(t))?
        }
        f.write_char(']')
    }

    pub fn debug_fmt(&self, t: ValueType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('[')?;
        for (i, e) in self.vec.borrow().iter().enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            write!(f, "{:#?}", e.clone().into_value(t))?
        }
        f.write_char(']')
    }
}

impl<T> Hash for ArrayObj<T>
where
    T: Element,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        for e in self.borrow_data().iter() {
            e.hash(state);
        }
    }
}

impl<T> Eq for ArrayObj<T> where T: Element + PartialEq {}

impl<T> PartialEq for ArrayObj<T>
where
    T: Element + PartialEq,
{
    fn eq(&self, b: &ArrayObj<T>) -> bool {
        if self.borrow_data().len() != b.borrow_data().len() {
            return false;
        }
        let bdata = b.borrow_data();
        for (i, e) in self.borrow_data().iter().enumerate() {
            if e != bdata.get(i).unwrap() {
                return false;
            }
        }
        true
    }
}

impl<T> Clone for ArrayObj<T>
where
    T: Element + PartialEq,
{
    fn clone(&self) -> Self {
        ArrayObj {
            vec: RefCell::new(self.borrow_data().iter().map(|x| x.clone()).collect()),
        }
    }
}

// ----------------------------------------------------------------------------
// SliceObj

#[derive(Clone)]
pub struct SliceObj<T> {
    array: GosValue,
    begin: Cell<usize>,
    end: Cell<usize>,
    // This is not capacity, but rather the max index that can be sliced.
    cap_end: Cell<usize>,
    phantom: PhantomData<T>,
}

pub type GosSliceObj = SliceObj<GosElem>;

impl<T> SliceObj<T>
where
    T: Element,
{
    pub fn with_array(arr: GosValue, begin: isize, end: isize) -> RuntimeResult<SliceObj<T>> {
        let len = arr.as_array::<T>().0.len();
        let (bi, ei, cap) = SliceObj::<T>::check_indices(0, len, len, begin, end, -1)?;
        Ok(SliceObj {
            begin: Cell::from(bi),
            end: Cell::from(ei),
            cap_end: Cell::from(cap),
            array: arr,
            phantom: PhantomData,
        })
    }

    /// Get a reference to the slice obj's array.
    #[must_use]
    #[inline]
    pub fn array(&self) -> &GosValue {
        &self.array
    }

    #[inline(always)]
    pub fn array_obj(&self) -> &ArrayObj<T> {
        &self.array.as_array::<T>().0
    }

    #[inline(always)]
    pub fn begin(&self) -> usize {
        self.begin.get()
    }

    #[inline(always)]
    pub fn end(&self) -> usize {
        self.end.get()
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.end() - self.begin()
    }

    #[inline(always)]
    pub fn cap(&self) -> usize {
        self.cap_end.get() - self.begin()
    }

    #[inline(always)]
    pub fn range(&self) -> Range<usize> {
        self.begin.get()..self.end.get()
    }

    #[inline]
    pub fn as_rust_slice(&self) -> Ref<[T]> {
        Ref::map(self.borrow_all_data(), |x| {
            &x[self.begin.get()..self.end.get()]
        })
    }

    #[inline]
    pub fn as_rust_slice_mut(&self) -> RefMut<[T]> {
        RefMut::map(self.borrow_all_data_mut(), |x| {
            &mut x[self.begin.get()..self.end.get()]
        })
    }

    #[inline]
    pub unsafe fn as_raw_slice<U>(&self) -> Ref<[U]> {
        assert!(std::mem::size_of::<U>() == std::mem::size_of::<T>());
        std::mem::transmute(self.as_rust_slice())
    }

    #[inline]
    pub unsafe fn as_raw_slice_mut<U>(&self) -> RefMut<[U]> {
        assert!(std::mem::size_of::<U>() == std::mem::size_of::<T>());
        std::mem::transmute(self.as_rust_slice_mut())
    }

    #[inline]
    pub fn sharing_with(&self, other: &SliceObj<T>) -> bool {
        self.array.as_addr() == other.array.as_addr()
    }

    /// get_array_equivalent returns the underlying array and mapped index
    #[inline]
    pub fn get_array_equivalent(&self, i: usize) -> (&GosValue, usize) {
        (self.array(), self.begin() + i)
    }

    #[inline(always)]
    pub fn index_elem(&self, i: usize) -> T {
        self.array_obj().index_elem(self.begin() + i)
    }

    #[inline(always)]
    pub fn get(&self, i: usize, t: ValueType) -> RuntimeResult<GosValue> {
        self.array_obj().get(self.begin() + i, t)
    }

    #[inline(always)]
    pub fn set(&self, i: usize, val: &GosValue) -> RuntimeResult<()> {
        self.array_obj().set(i, val)
    }

    #[inline]
    pub fn push(&mut self, val: GosValue) {
        let mut data = self.borrow_all_data_mut();
        if data.len() == self.len() {
            data.push(T::from_value(val))
        } else {
            data[self.end()] = T::from_value(val);
        }
        drop(data);
        *self.end.get_mut() += 1;
        if self.cap_end.get() < self.end.get() {
            *self.cap_end.get_mut() += 1;
        }
    }

    #[inline]
    pub fn append(&mut self, other: &SliceObj<T>) {
        let mut data = self.borrow_all_data_mut();
        let new_end = self.end() + other.len();
        let after_end_len = data.len() - self.end();
        let sharing = self.sharing_with(other);
        if after_end_len <= other.len() {
            data.truncate(self.end());
            if !sharing {
                data.extend_from_slice(&other.as_rust_slice());
            } else {
                data.extend_from_within(other.range());
            }
        } else {
            if !sharing {
                T::copy_or_clone_slice(&mut data[self.end()..new_end], &other.as_rust_slice());
            } else {
                let cloned = data[other.range()].to_vec();
                T::copy_or_clone_slice(&mut data[self.end()..new_end], &cloned);
            }
        }
        drop(data);
        *self.end.get_mut() = new_end;
        if self.cap_end.get() < self.end.get() {
            *self.cap_end.get_mut() = self.end.get();
        }
    }

    #[inline]
    pub fn copy_from(&self, other: &SliceObj<T>) -> usize {
        let other_range = other.range();
        let (left, right) = match self.len() >= other.len() {
            true => (self.begin()..self.begin() + other.len(), other_range),
            false => (
                self.begin()..self.end(),
                other_range.start..other_range.start + self.len(),
            ),
        };
        let len = left.len();
        let sharing = self.sharing_with(other);
        let data = &mut self.borrow_all_data_mut();
        if !sharing {
            T::copy_or_clone_slice(&mut data[left], &other.borrow_all_data()[right]);
        } else {
            let cloned = data[right].to_vec();
            T::copy_or_clone_slice(&mut data[left], &cloned);
        }
        len
    }

    #[inline]
    pub fn slice(&self, begin: isize, end: isize, max: isize) -> RuntimeResult<SliceObj<T>> {
        let (bi, ei, cap) = SliceObj::<T>::check_indices(
            self.begin(),
            self.len(),
            self.cap_end.get(),
            begin,
            end,
            max,
        )?;
        Ok(SliceObj {
            begin: Cell::from(bi),
            end: Cell::from(ei),
            cap_end: Cell::from(cap),
            array: self.array.clone(),
            phantom: PhantomData,
        })
    }

    #[inline]
    pub fn get_vec(&self, t: ValueType) -> Vec<GosValue> {
        self.as_rust_slice()
            .iter()
            .map(|x: &T| x.clone().into_value(t))
            .collect()
    }

    #[inline]
    pub fn swap(&self, i: usize, j: usize) -> RuntimeResult<()> {
        let len = self.len();
        if i >= len {
            Err(format!("index {} out of range", i))
        } else if j >= len {
            Err(format!("index {} out of range", j))
        } else {
            self.borrow_all_data_mut()
                .swap(i + self.begin(), j + self.begin());
            Ok(())
        }
    }

    #[inline]
    pub fn borrow_all_data_mut(&self) -> std::cell::RefMut<Vec<T>> {
        self.array_obj().borrow_data_mut()
    }

    #[inline]
    fn borrow_all_data(&self) -> std::cell::Ref<Vec<T>> {
        self.array_obj().borrow_data()
    }

    #[inline]
    fn check_indices(
        this_begin: usize,
        this_len: usize,
        this_cap: usize,
        begin: isize,
        end: isize,
        max: isize,
    ) -> RuntimeResult<(usize, usize, usize)> {
        let bi = this_begin + begin as usize;
        if bi > this_cap {
            return Err(format!("index {} out of range", begin).to_owned());
        }
        let this_len_p1 = this_len as isize + 1;
        let ei = this_begin + ((this_len_p1 + end) % this_len_p1) as usize;
        if ei < bi {
            return Err(format!("index {} out of range", end).to_owned());
        }
        let cap = if max < 0 { this_cap } else { max as usize };
        if cap > this_cap {
            return Err(format!("index {} out of range", max).to_owned());
        }
        Ok((bi, ei, cap))
    }

    pub fn display_fmt(&self, t: ValueType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('[')?;
        for (i, e) in self.as_rust_slice().iter().enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            write!(f, "{}", e.clone().into_value(t))?
        }
        f.write_char(']')
    }

    pub fn debug_fmt(&self, t: ValueType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('[')?;
        for (i, e) in self.as_rust_slice().iter().enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            write!(f, "{:#?}", e.clone().into_value(t))?
        }
        f.write_char(']')
    }
}

impl<T> PartialEq for SliceObj<T> {
    fn eq(&self, _other: &SliceObj<T>) -> bool {
        unreachable!() //false
    }
}

impl<T> Eq for SliceObj<T> {}

pub type SliceIter<'a, T> = std::slice::Iter<'a, T>;

pub type SliceEnumIter<'a, T> = std::iter::Enumerate<SliceIter<'a, T>>;

// ----------------------------------------------------------------------------
// StringObj

pub type StringIter<'a> = std::str::Chars<'a>;

pub type StringEnumIter<'a> = std::iter::Enumerate<StringIter<'a>>;

pub type StringObj = SliceObj<Elem8>;

pub struct StrUtil;

impl StrUtil {
    #[inline]
    pub fn with_str(s: &str) -> StringObj {
        let buf: Vec<Elem8> = unsafe { std::mem::transmute(s.as_bytes().to_vec()) };
        StrUtil::buf_into_string(buf)
    }

    /// It's safe because strings are readonly
    /// https://stackoverflow.com/questions/50431702/is-it-safe-and-defined-behavior-to-transmute-between-a-t-and-an-unsafecellt
    /// https://doc.rust-lang.org/src/core/str/converts.rs.html#173
    #[inline]
    pub fn as_str(this: &StringObj) -> Ref<str> {
        unsafe { std::mem::transmute(this.as_rust_slice()) }
    }

    #[inline]
    pub fn index(this: &StringObj, i: usize) -> RuntimeResult<GosValue> {
        this.get(i, ValueType::Uint8)
    }

    #[inline]
    pub fn index_elem(this: &StringObj, i: usize) -> u8 {
        this.index_elem(i).into_inner()
    }

    #[inline]
    pub fn add(a: &StringObj, b: &StringObj) -> StringObj {
        let mut buf = a.as_rust_slice().to_vec();
        buf.extend_from_slice(&b.as_rust_slice());
        StrUtil::buf_into_string(buf)
    }

    #[inline]
    fn buf_into_string(buf: Vec<Elem8>) -> StringObj {
        let arr = GosValue::new_non_gc_array(ArrayObj::with_raw_data(buf), ValueType::Uint8);
        SliceObj::with_array(arr, 0, -1).unwrap()
    }
}

// ----------------------------------------------------------------------------
// StructObj

#[derive(Clone, Debug)]
pub struct StructObj {
    fields: RefCell<Vec<GosValue>>,
}

impl StructObj {
    pub fn new(fields: Vec<GosValue>) -> StructObj {
        StructObj {
            fields: RefCell::new(fields),
        }
    }

    #[inline]
    pub fn borrow_fields(&self) -> Ref<Vec<GosValue>> {
        self.fields.borrow()
    }

    #[inline]
    pub fn borrow_fields_mut(&self) -> RefMut<Vec<GosValue>> {
        self.fields.borrow_mut()
    }
}

impl Eq for StructObj {}

impl PartialEq for StructObj {
    #[inline]
    fn eq(&self, other: &StructObj) -> bool {
        let other_fields = other.borrow_fields();
        for (i, f) in self.borrow_fields().iter().enumerate() {
            if f != &other_fields[i] {
                return false;
            }
        }
        return true;
    }
}

impl Hash for StructObj {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        for f in self.borrow_fields().iter() {
            f.hash(state)
        }
    }
}

impl Display for StructObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('{')?;
        for (i, fld) in self.borrow_fields().iter().enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            write!(f, "{}", fld)?
        }
        f.write_char('}')
    }
}

// ----------------------------------------------------------------------------
// InterfaceObj

#[derive(Clone, Debug)]
pub struct UnderlyingFfi {
    pub ffi_obj: Rc<dyn Ffi>,
    pub meta: Meta,
}

impl UnderlyingFfi {
    pub fn new(obj: Rc<dyn Ffi>, meta: Meta) -> UnderlyingFfi {
        UnderlyingFfi {
            ffi_obj: obj,
            meta: meta,
        }
    }
}

/// Info about how to invoke a method of the underlying value
/// of an interface
#[derive(Clone, Debug)]
pub enum IfaceBinding {
    Struct(Rc<RefCell<MethodDesc>>, Option<Vec<usize>>),
    Iface(usize, Option<Vec<usize>>),
}

#[derive(Clone, Debug)]
pub enum Binding4Runtime {
    Struct(FunctionKey, bool, Option<Vec<usize>>),
    Iface(usize, Option<Vec<usize>>),
}

impl From<IfaceBinding> for Binding4Runtime {
    fn from(item: IfaceBinding) -> Binding4Runtime {
        match item {
            IfaceBinding::Struct(f, indices) => {
                let md = f.borrow();
                Binding4Runtime::Struct(md.func.unwrap(), md.pointer_recv, indices)
            }
            IfaceBinding::Iface(a, b) => Binding4Runtime::Iface(a, b),
        }
    }
}

#[derive(Clone, Debug)]
pub enum InterfaceObj {
    // The Meta and Binding info are all determined at compile time.
    // They are not available if the Interface is created at runtime
    // as an empty Interface holding a GosValue, which acts like a
    // dynamic type.
    Gos(GosValue, Option<(Meta, Vec<Binding4Runtime>)>),
    Ffi(UnderlyingFfi),
}

impl InterfaceObj {
    pub fn with_value(val: GosValue, meta: Option<(Meta, Vec<Binding4Runtime>)>) -> InterfaceObj {
        InterfaceObj::Gos(val, meta)
    }

    #[inline]
    pub fn underlying_value(&self) -> Option<&GosValue> {
        match self {
            Self::Gos(v, _) => Some(v),
            _ => None,
        }
    }

    #[inline]
    pub fn equals_value(&self, val: &GosValue) -> bool {
        match self.underlying_value() {
            Some(v) => v == val,
            None => false,
        }
    }

    /// for gc
    pub fn ref_sub_one(&self) {
        match self {
            Self::Gos(v, _) => v.ref_sub_one(),
            _ => {}
        };
    }

    /// for gc
    pub fn mark_dirty(&self, queue: &mut RCQueue) {
        match self {
            Self::Gos(v, _) => v.mark_dirty(queue),
            _ => {}
        };
    }
}

impl Eq for InterfaceObj {}

impl PartialEq for InterfaceObj {
    #[inline]
    fn eq(&self, other: &InterfaceObj) -> bool {
        match (self, other) {
            (Self::Gos(x, _), Self::Gos(y, _)) => x == y,
            (Self::Ffi(x), Self::Ffi(y)) => Rc::ptr_eq(&x.ffi_obj, &y.ffi_obj),
            _ => false,
        }
    }
}

impl Hash for InterfaceObj {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Gos(v, _) => v.hash(state),
            Self::Ffi(ffi) => Rc::as_ptr(&ffi.ffi_obj).hash(state),
        }
    }
}

impl Display for InterfaceObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gos(v, _) => write!(f, "{}", v),
            Self::Ffi(ffi) => write!(f, "<ffi>{:?}", ffi.ffi_obj),
        }
    }
}

// ----------------------------------------------------------------------------
// ChannelObj

#[derive(Clone, Debug)]
pub struct ChannelObj {
    pub recv_zero: GosValue,
    pub chan: Channel,
}

impl ChannelObj {
    pub fn new(cap: usize, recv_zero: GosValue) -> ChannelObj {
        ChannelObj {
            chan: Channel::new(cap),
            recv_zero: recv_zero,
        }
    }

    pub fn with_chan(chan: Channel, recv_zero: GosValue) -> ChannelObj {
        ChannelObj {
            chan: chan,
            recv_zero: recv_zero,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.chan.len()
    }

    #[inline]
    pub fn cap(&self) -> usize {
        self.chan.cap()
    }

    #[inline]
    pub fn close(&self) {
        self.chan.close()
    }

    pub async fn send(&self, v: &GosValue) -> RuntimeResult<()> {
        self.chan.send(v).await
    }

    pub async fn recv(&self) -> Option<GosValue> {
        self.chan.recv().await
    }
}

// ----------------------------------------------------------------------------
// PointerObj

/// There are 4 types of pointers, they point to:
/// - local
/// - slice member
/// - struct field
/// - package member
/// References to outer function's local vars and pointers to local vars are all
/// UpValues

#[derive(Debug, Clone)]
pub enum PointerObj {
    UpVal(UpValue),
    SliceMember(GosValue, OpIndex),
    StructField(GosValue, OpIndex),
    PkgMember(PackageKey, OpIndex),
}

impl PointerObj {
    #[inline]
    pub fn new_closed_up_value(val: &GosValue) -> PointerObj {
        PointerObj::UpVal(UpValue::new_closed(val.clone()))
    }

    #[inline]
    pub fn new_array_member(
        val: GosValue,
        i: OpIndex,
        t_elem: ValueType,
    ) -> RuntimeResult<PointerObj> {
        let slice = GosValue::slice_array(val, 0, -1, t_elem)?;
        // todo: index check!
        Ok(PointerObj::SliceMember(slice, i))
    }

    #[inline]
    pub fn new_slice_member(
        val: GosValue,
        i: OpIndex,
        t: ValueType,
        t_elem: ValueType,
    ) -> RuntimeResult<PointerObj> {
        match t {
            ValueType::Array => PointerObj::new_array_member(val, i, t_elem),
            ValueType::Slice => match val.is_nil() {
                false => Ok(PointerObj::SliceMember(val, i)),
                true => Err("access nil value".to_owned()),
            },
            _ => unreachable!(),
        }
    }

    #[inline]
    pub fn deref(&self, stack: &Stack, pkgs: &PackageObjs) -> RuntimeResult<GosValue> {
        match self {
            PointerObj::UpVal(uv) => Ok(uv.value(stack)),
            PointerObj::SliceMember(s, index) => s.dispatcher_a_s().slice_get(s, *index as usize),
            PointerObj::StructField(s, index) => {
                Ok(s.as_struct().0.borrow_fields()[*index as usize].clone())
            }
            PointerObj::PkgMember(pkg, index) => Ok(pkgs[*pkg].member(*index).clone()),
        }
    }

    /// cast returns the GosValue the pointer points to, with a new type
    /// obviously this is very unsafe, more rigorous checks should be done here.
    #[inline]
    pub fn cast(
        &self,
        new_type: ValueType,
        stack: &Stack,
        pkgs: &PackageObjs,
    ) -> RuntimeResult<PointerObj> {
        let val = self.deref(stack, pkgs)?;
        let val = unsafe { val.cast(new_type) };
        Ok(PointerObj::new_closed_up_value(&val))
    }

    /// set_value is not used by VM, it's for FFI
    pub fn set_pointee(
        &self,
        val: &GosValue,
        stack: &mut Stack,
        pkgs: &PackageObjs,
        gcv: &GcoVec,
    ) -> RuntimeResult<()> {
        match self {
            PointerObj::UpVal(uv) => uv.set_value(val.copy_semantic(gcv), stack),
            PointerObj::SliceMember(s, index) => {
                s.dispatcher_a_s()
                    .slice_set(s, &val.copy_semantic(gcv), *index as usize)?;
            }
            PointerObj::StructField(s, index) => {
                let target: &mut GosValue =
                    &mut s.as_struct().0.borrow_fields_mut()[*index as usize];
                *target = val.copy_semantic(gcv);
            }
            PointerObj::PkgMember(p, index) => {
                let target: &mut GosValue = &mut pkgs[*p].member_mut(*index);
                *target = val.copy_semantic(gcv);
            }
        }
        Ok(())
    }

    /// for gc
    pub fn ref_sub_one(&self) {
        match &self {
            PointerObj::UpVal(uv) => uv.ref_sub_one(),
            PointerObj::SliceMember(s, _) => {
                let rc = &s.as_gos_slice().unwrap().1;
                rc.set(rc.get() - 1);
            }
            PointerObj::StructField(s, _) => {
                let rc = &s.as_struct().1;
                rc.set(rc.get() - 1);
            }
            _ => {}
        };
    }

    /// for gc
    pub fn mark_dirty(&self, queue: &mut RCQueue) {
        match &self {
            PointerObj::UpVal(uv) => uv.mark_dirty(queue),
            PointerObj::SliceMember(s, _) => {
                rcount_mark_and_queue(&s.as_gos_slice().unwrap().1, queue)
            }
            PointerObj::StructField(s, _) => rcount_mark_and_queue(&s.as_struct().1, queue),
            _ => {}
        };
    }
}

impl Eq for PointerObj {}

impl PartialEq for PointerObj {
    #[inline]
    fn eq(&self, other: &PointerObj) -> bool {
        match (self, other) {
            (Self::UpVal(x), Self::UpVal(y)) => x == y,
            (Self::SliceMember(x, ix), Self::SliceMember(y, iy)) => {
                x.as_addr() == y.as_addr() && ix == iy
            }
            (Self::StructField(x, ix), Self::StructField(y, iy)) => {
                x.as_addr() == y.as_addr() && ix == iy
            }
            (Self::PkgMember(ka, ix), Self::PkgMember(kb, iy)) => ka == kb && ix == iy,
            _ => false,
        }
    }
}

impl Hash for PointerObj {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::UpVal(x) => x.hash(state),
            Self::SliceMember(s, index) => {
                s.as_addr().hash(state);
                index.hash(state);
            }
            Self::StructField(s, index) => {
                s.as_addr().hash(state);
                index.hash(state);
            }
            Self::PkgMember(p, index) => {
                p.hash(state);
                index.hash(state);
            }
        }
    }
}

impl Display for PointerObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UpVal(uv) => write!(f, "{:p}", Rc::as_ptr(&uv.inner)),
            Self::SliceMember(s, i) => write!(f, "{:p}i{}", s.as_addr(), i),
            Self::StructField(s, i) => write!(f, "{:p}i{}", s.as_addr(), i),
            Self::PkgMember(p, i) => write!(f, "{:x}i{}", key_to_u64(*p), i),
        }
    }
}

// ----------------------------------------------------------------------------
// UnsafePtrObj

pub trait UnsafePtr {
    /// For downcasting
    fn as_any(&self) -> &dyn Any;

    fn eq(&self, _: &dyn UnsafePtr) -> bool {
        panic!("implement your own eq for your type");
    }

    /// for gc
    fn ref_sub_one(&self) {}

    /// for gc
    fn mark_dirty(&self, _: &mut RCQueue) {}

    /// Returns true if the user data can make reference cycles, so that GC can
    fn can_make_cycle(&self) -> bool {
        false
    }

    /// If can_make_cycle returns true, implement this to break cycle
    fn break_cycle(&self) {}
}

impl std::fmt::Debug for dyn UnsafePtr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", "unsafe pointer")
    }
}

/// PointerHandle is used when converting a runtime pointer to a unsafe.Default
#[derive(Debug, Clone)]
pub struct PointerHandle {
    ptr: PointerObj,
}

impl UnsafePtr for PointerHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn eq(&self, other: &dyn UnsafePtr) -> bool {
        let a = self.as_any().downcast_ref::<PointerHandle>();
        let b = other.as_any().downcast_ref::<PointerHandle>();
        match b {
            Some(h) => h.ptr == a.unwrap().ptr,
            None => false,
        }
    }

    /// for gc
    fn ref_sub_one(&self) {
        self.ptr.ref_sub_one()
    }

    /// for gc
    fn mark_dirty(&self, q: &mut RCQueue) {
        self.ptr.mark_dirty(q)
    }
}

impl PointerHandle {
    pub fn new(ptr: &GosValue) -> GosValue {
        match ptr.as_pointer() {
            Some(p) => {
                let handle = PointerHandle { ptr: p.clone() };
                GosValue::new_unsafe_ptr(handle)
            }
            None => GosValue::new_nil(ValueType::UnsafePtr),
        }
    }

    /// Get a reference to the pointer handle's ptr.
    pub fn ptr(&self) -> &PointerObj {
        &self.ptr
    }
}

#[derive(Debug, Clone)]
pub struct UnsafePtrObj {
    ptr: Rc<dyn UnsafePtr>,
}

impl UnsafePtrObj {
    pub fn new<T: 'static + UnsafePtr>(p: T) -> UnsafePtrObj {
        UnsafePtrObj { ptr: Rc::new(p) }
    }

    /// Get a reference to the unsafe ptr obj's ptr.
    pub fn ptr(&self) -> &dyn UnsafePtr {
        &*self.ptr
    }

    pub fn as_rust_ptr(&self) -> *const dyn UnsafePtr {
        &*self.ptr
    }

    pub fn downcast_ref<T: Any>(&self) -> RuntimeResult<&T> {
        self.ptr
            .as_any()
            .downcast_ref::<T>()
            .ok_or("Unexpected unsafe pointer type".to_owned())
    }
}

impl Eq for UnsafePtrObj {}

impl PartialEq for UnsafePtrObj {
    #[inline]
    fn eq(&self, other: &UnsafePtrObj) -> bool {
        self.ptr.eq(other.ptr())
    }
}

impl Display for UnsafePtrObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:p}", &*self.ptr as *const dyn UnsafePtr)
    }
}
// ----------------------------------------------------------------------------
// ClosureObj

#[derive(Clone)]
pub struct ValueDesc {
    pub func: FunctionKey,
    pub index: OpIndex,
    pub typ: ValueType,
    pub is_up_value: bool,
    pub stack: Weak<RefCell<Stack>>,
    pub stack_base: OpIndex,
}

impl Eq for ValueDesc {}

impl PartialEq for ValueDesc {
    #[inline]
    fn eq(&self, other: &ValueDesc) -> bool {
        self.stack_base + self.index == other.stack_base + other.index
            && self.stack.ptr_eq(&other.stack)
    }
}

impl ValueDesc {
    pub fn new(func: FunctionKey, index: OpIndex, typ: ValueType, is_up_value: bool) -> ValueDesc {
        ValueDesc {
            func: func,
            index: index,
            typ: typ,
            is_up_value: is_up_value,
            stack: Weak::new(),
            stack_base: 0,
        }
    }

    #[inline]
    pub fn clone_with_stack(&self, stack: Weak<RefCell<Stack>>, stack_base: OpIndex) -> ValueDesc {
        ValueDesc {
            func: self.func,
            index: self.index,
            typ: self.typ,
            is_up_value: self.is_up_value,
            stack: stack,
            stack_base: stack_base,
        }
    }

    #[inline]
    pub fn abs_index(&self) -> usize {
        (self.stack_base + self.index) as usize
    }

    #[inline]
    pub fn load(&self, stack: &Stack) -> GosValue {
        let index = self.abs_index();
        let uv_stack = self.stack.upgrade().unwrap();
        if ptr::eq(uv_stack.as_ptr(), stack) {
            stack.get(index).clone()
        } else {
            uv_stack.borrow().get(index).clone()
        }
    }

    #[inline]
    pub fn store(&self, val: GosValue, stack: &mut Stack) {
        let index = self.abs_index();
        let uv_stack = self.stack.upgrade().unwrap();
        if ptr::eq(uv_stack.as_ptr(), stack) {
            stack.set(index, val);
        } else {
            uv_stack.borrow_mut().set(index, val);
        }
    }
}

impl std::fmt::Debug for ValueDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("ValueDesc")
            .field("type", &self.typ)
            .field("is_up_value", &self.is_up_value)
            .field("index", &self.abs_index())
            .field("stack", &self.stack.as_ptr())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum UpValueState {
    /// Parent CallFrame is still alive, pointing to a local variable
    Open(ValueDesc), // (what func is the var defined, the index of the var)
    // Parent CallFrame is released, pointing to a pointer value in the global pool
    Closed(GosValue),
}

impl std::fmt::Debug for UpValueState {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self {
            Self::Open(desc) => write!(f, "UpValue::Open(  {:#?} )", desc),
            Self::Closed(v) => write!(f, "UpValue::Closed(  {:#018x} )", v.data().as_uint()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UpValue {
    pub inner: Rc<RefCell<UpValueState>>,
}

impl UpValue {
    pub fn new(d: ValueDesc) -> UpValue {
        UpValue {
            inner: Rc::new(RefCell::new(UpValueState::Open(d))),
        }
    }

    pub fn new_closed(v: GosValue) -> UpValue {
        UpValue {
            inner: Rc::new(RefCell::new(UpValueState::Closed(v))),
        }
    }

    pub fn is_open(&self) -> bool {
        match &self.inner.borrow() as &UpValueState {
            UpValueState::Open(_) => true,
            UpValueState::Closed(_) => false,
        }
    }

    pub fn downgrade(&self) -> WeakUpValue {
        WeakUpValue {
            inner: Rc::downgrade(&self.inner),
        }
    }

    pub fn desc(&self) -> ValueDesc {
        let r: &UpValueState = &self.inner.borrow();
        match r {
            UpValueState::Open(d) => d.clone(),
            _ => unreachable!(),
        }
    }

    pub fn close(&self, val: GosValue) {
        *self.inner.borrow_mut() = UpValueState::Closed(val);
    }

    pub fn value(&self, stack: &Stack) -> GosValue {
        match &self.inner.borrow() as &UpValueState {
            UpValueState::Open(desc) => desc.load(stack),
            UpValueState::Closed(val) => val.clone(),
        }
    }

    pub fn set_value(&self, val: GosValue, stack: &mut Stack) {
        match &mut self.inner.borrow_mut() as &mut UpValueState {
            UpValueState::Open(desc) => desc.store(val, stack),
            UpValueState::Closed(v) => {
                *v = val;
            }
        }
    }

    /// for gc
    pub fn ref_sub_one(&self) {
        let state: &UpValueState = &self.inner.borrow();
        if let UpValueState::Closed(uvs) = state {
            uvs.ref_sub_one()
        }
    }

    /// for gc
    pub fn mark_dirty(&self, queue: &mut RCQueue) {
        let state: &UpValueState = &self.inner.borrow();
        if let UpValueState::Closed(uvs) = state {
            uvs.mark_dirty(queue)
        }
    }
}

impl Eq for UpValue {}

impl PartialEq for UpValue {
    #[inline]
    fn eq(&self, b: &UpValue) -> bool {
        let state_a: &UpValueState = &self.inner.borrow();
        let state_b: &UpValueState = &b.inner.borrow();
        match (state_a, state_b) {
            (UpValueState::Closed(va), UpValueState::Closed(vb)) => {
                if va.typ().copyable() {
                    Rc::ptr_eq(&self.inner, &b.inner)
                } else {
                    va.data().as_addr() == vb.data().as_addr()
                }
            }
            (UpValueState::Open(da), UpValueState::Open(db)) => {
                da.abs_index() == db.abs_index() && Weak::ptr_eq(&da.stack, &db.stack)
            }
            _ => false,
        }
    }
}

impl Hash for UpValue {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        let b: &UpValueState = &self.inner.borrow();
        match b {
            UpValueState::Open(desc) => desc.abs_index().hash(state),
            UpValueState::Closed(_) => Rc::as_ptr(&self.inner).hash(state),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WeakUpValue {
    pub inner: Weak<RefCell<UpValueState>>,
}

impl WeakUpValue {
    pub fn upgrade(&self) -> Option<UpValue> {
        Weak::upgrade(&self.inner).map(|x| UpValue { inner: x })
    }
}

#[derive(Clone, Debug)]
pub struct GosClosureObj {
    pub func: FunctionKey,
    pub uvs: Option<HashMap<usize, UpValue>>,
    pub recv: Option<GosValue>,
    pub meta: Meta,
}

#[derive(Clone, Debug)]
pub struct FfiClosureObj {
    pub ffi: Rc<dyn Ffi>,
    pub func_name: String,
    pub meta: Meta,
}

#[derive(Clone, Debug)]
pub enum ClosureObj {
    Gos(GosClosureObj),
    Ffi(FfiClosureObj),
}

impl ClosureObj {
    pub fn new_gos(key: FunctionKey, fobjs: &FunctionObjs, recv: Option<GosValue>) -> ClosureObj {
        let func = &fobjs[key];
        let uvs: Option<HashMap<usize, UpValue>> = if func.up_ptrs.len() > 0 {
            Some(
                func.up_ptrs
                    .iter()
                    .enumerate()
                    .filter(|(_, x)| x.is_up_value)
                    .map(|(i, x)| (i, UpValue::new(x.clone())))
                    .collect(),
            )
        } else {
            None
        };
        ClosureObj::Gos(GosClosureObj {
            func: key,
            uvs: uvs,
            recv: recv,
            meta: func.meta,
        })
    }

    #[inline]
    pub fn new_ffi(ffi: FfiClosureObj) -> ClosureObj {
        ClosureObj::Ffi(ffi)
    }

    #[inline]
    pub fn as_gos(&self) -> &GosClosureObj {
        match self {
            Self::Gos(c) => c,
            _ => unreachable!(),
        }
    }

    #[inline]
    pub fn total_param_count(&self, metas: &MetadataObjs) -> usize {
        match self {
            Self::Gos(g) => {
                let sig = &metas[g.meta.key].as_signature();
                sig.params.len() + sig.results.len()
            }
            Self::Ffi(f) => {
                let sig = &metas[f.meta.key].as_signature();
                sig.params.len()
            }
        }
    }

    /// for gc
    pub fn ref_sub_one(&self) {
        match self {
            Self::Gos(obj) => {
                if let Some(uvs) = &obj.uvs {
                    for (_, v) in uvs.iter() {
                        v.ref_sub_one()
                    }
                }
                if let Some(recv) = &obj.recv {
                    recv.ref_sub_one()
                }
            }
            Self::Ffi(_) => {}
        }
    }

    /// for gc
    pub fn mark_dirty(&self, queue: &mut RCQueue) {
        match self {
            Self::Gos(obj) => {
                if let Some(uvs) = &obj.uvs {
                    for (_, v) in uvs.iter() {
                        v.mark_dirty(queue)
                    }
                }
                if let Some(recv) = &obj.recv {
                    recv.mark_dirty(queue)
                }
            }
            Self::Ffi(_) => {}
        }
    }
}

// ----------------------------------------------------------------------------
// PackageVal

/// PackageVal is part of the generated Bytecode, it stores imports, consts,
/// vars, funcs declared in a package
#[derive(Clone, Debug)]
pub struct PackageVal {
    members: Vec<Rc<RefCell<GosValue>>>, // imports, const, var, func are all stored here
    member_types: Vec<ValueType>,
    member_indices: HashMap<String, OpIndex>,
    init_funcs: Vec<GosValue>,
    // maps func_member_index of the constructor to pkg_member_index
    var_mapping: Option<HashMap<OpIndex, OpIndex>>,
}

impl PackageVal {
    pub fn new() -> PackageVal {
        PackageVal {
            members: vec![],
            member_types: vec![],
            member_indices: HashMap::new(),
            init_funcs: vec![],
            var_mapping: Some(HashMap::new()),
        }
    }

    pub fn add_member(&mut self, name: String, val: GosValue, typ: ValueType) -> OpIndex {
        self.members.push(Rc::new(RefCell::new(val)));
        self.member_types.push(typ);
        let index = (self.members.len() - 1) as OpIndex;
        self.member_indices.insert(name, index);
        index as OpIndex
    }

    pub fn add_var_mapping(&mut self, name: String, fn_index: OpIndex) -> OpIndex {
        let index = *self.get_member_index(&name).unwrap();
        self.var_mapping.as_mut().unwrap().insert(fn_index, index);
        index
    }

    pub fn add_init_func(&mut self, func: GosValue) {
        self.init_funcs.push(func);
    }

    pub fn get_member_index(&self, name: &str) -> Option<&OpIndex> {
        self.member_indices.get(name)
    }

    pub fn inited(&self) -> bool {
        self.var_mapping.is_none()
    }

    pub fn set_inited(&mut self) {
        self.var_mapping = None
    }

    #[inline]
    pub fn member(&self, i: OpIndex) -> Ref<GosValue> {
        self.members[i as usize].borrow()
    }

    #[inline]
    pub fn member_mut(&self, i: OpIndex) -> RefMut<GosValue> {
        self.members[i as usize].borrow_mut()
    }

    #[inline]
    pub fn init_func(&self, i: OpIndex) -> Option<&GosValue> {
        self.init_funcs.get(i as usize)
    }

    #[inline]
    pub fn init_vars(&self, stack: &mut Stack) {
        let mapping = self.var_mapping.as_ref().unwrap();
        let count = mapping.len();
        for i in 0..count {
            let vi = mapping[&((count - 1 - i) as OpIndex)];
            *self.member_mut(vi) = stack.pop_value();
        }
    }
}

// ----------------------------------------------------------------------------
// FunctionVal

/// EntIndex is for addressing a variable in the scope of a function
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EntIndex {
    Const(OpIndex),
    LocalVar(OpIndex),
    UpValue(OpIndex),
    PackageMember(PackageKey, KeyData),
    BuiltInVal(Opcode), // built-in identifiers
    TypeMeta(Meta),
    Blank,
}

impl From<EntIndex> for OpIndex {
    fn from(t: EntIndex) -> OpIndex {
        match t {
            EntIndex::Const(i) => i,
            EntIndex::LocalVar(i) => i,
            EntIndex::UpValue(i) => i,
            EntIndex::PackageMember(_, _) => unreachable!(),
            EntIndex::BuiltInVal(_) => unreachable!(),
            EntIndex::TypeMeta(_) => unreachable!(),
            EntIndex::Blank => unreachable!(),
        }
    }
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
pub enum FuncFlag {
    Default,
    PkgCtor,
    HasDefer,
}

/// FunctionVal is the direct container of the Opcode.
#[derive(Clone, Debug)]
pub struct FunctionVal {
    pub package: PackageKey,
    pub meta: Meta,
    code: Vec<Instruction>,
    pos: Vec<Option<usize>>,
    pub consts: Vec<GosValue>,
    pub up_ptrs: Vec<ValueDesc>,

    pub stack_temp_types: Vec<ValueType>,
    pub ret_zeros: Vec<GosValue>,
    pub local_zeros: Vec<GosValue>,
    pub flag: FuncFlag,

    entities: HashMap<KeyData, EntIndex>,
    uv_entities: HashMap<KeyData, EntIndex>,
    local_alloc: OpIndex,
}

impl FunctionVal {
    pub fn new(
        package: PackageKey,
        meta: Meta,
        metas: &MetadataObjs,
        gcv: &GcoVec,
        flag: FuncFlag,
    ) -> FunctionVal {
        let s = &metas[meta.key].as_signature();
        let returns = s.results.iter().map(|m| m.zero(metas, gcv)).collect();
        let mut p_types: Vec<ValueType> = s.recv.map_or(vec![], |m| vec![m.value_type(&metas)]);
        p_types.append(&mut s.params.iter().map(|m| m.value_type(&metas)).collect());

        FunctionVal {
            package: package,
            meta: meta,
            code: Vec::new(),
            pos: Vec::new(),
            consts: Vec::new(),
            up_ptrs: Vec::new(),
            stack_temp_types: p_types,
            ret_zeros: returns,
            local_zeros: Vec::new(),
            flag: flag,
            entities: HashMap::new(),
            uv_entities: HashMap::new(),
            local_alloc: 0,
        }
    }

    #[inline]
    pub fn code(&self) -> &Vec<Instruction> {
        &self.code
    }

    #[inline]
    pub fn instruction_mut(&mut self, i: usize) -> &mut Instruction {
        self.code.get_mut(i).unwrap()
    }

    #[inline]
    pub fn pos(&self) -> &Vec<Option<usize>> {
        &self.pos
    }

    #[inline]
    pub fn param_count(&self) -> usize {
        self.stack_temp_types.len() - self.local_zeros.len()
    }

    #[inline]
    pub fn param_types(&self) -> &[ValueType] {
        &self.stack_temp_types[..self.param_count()]
    }

    #[inline]
    pub fn ret_count(&self) -> usize {
        self.ret_zeros.len()
    }

    #[inline]
    pub fn is_ctor(&self) -> bool {
        self.flag == FuncFlag::PkgCtor
    }

    #[inline]
    pub fn local_count(&self) -> usize {
        self.local_alloc as usize - self.param_count() - self.ret_count()
    }

    #[inline]
    pub fn entity_index(&self, entity: &KeyData) -> Option<&EntIndex> {
        self.entities.get(entity)
    }

    #[inline]
    pub fn const_val(&self, index: OpIndex) -> &GosValue {
        &self.consts[index as usize]
    }

    #[inline]
    pub fn offset(&self, loc: usize) -> OpIndex {
        // todo: don't crash if OpIndex overflows
        OpIndex::try_from((self.code.len() - loc) as isize).unwrap()
    }

    #[inline]
    pub fn next_code_index(&self) -> usize {
        self.code.len()
    }

    #[inline]
    pub fn push_inst_pos(&mut self, i: Instruction, pos: Option<usize>) {
        self.code.push(i);
        self.pos.push(pos);
    }

    #[inline]
    pub fn emit_inst(
        &mut self,
        op: Opcode,
        types: [Option<ValueType>; 3],
        imm: Option<i32>,
        pos: Option<usize>,
    ) {
        let i = Instruction::new(op, types[0], types[1], types[2], imm);
        self.code.push(i);
        self.pos.push(pos);
    }

    pub fn emit_raw_inst(&mut self, u: u64, pos: Option<usize>) {
        let i = Instruction::from_u64(u);
        self.code.push(i);
        self.pos.push(pos);
    }

    pub fn emit_code_with_type(&mut self, code: Opcode, t: ValueType, pos: Option<usize>) {
        self.emit_inst(code, [Some(t), None, None], None, pos);
    }

    pub fn emit_code_with_type2(
        &mut self,
        code: Opcode,
        t0: ValueType,
        t1: Option<ValueType>,
        pos: Option<usize>,
    ) {
        self.emit_inst(code, [Some(t0), t1, None], None, pos);
    }

    pub fn emit_code_with_imm(&mut self, code: Opcode, imm: OpIndex, pos: Option<usize>) {
        self.emit_inst(code, [None, None, None], Some(imm), pos);
    }

    pub fn emit_code_with_type_imm(
        &mut self,
        code: Opcode,
        t: ValueType,
        imm: OpIndex,
        pos: Option<usize>,
    ) {
        self.emit_inst(code, [Some(t), None, None], Some(imm), pos);
    }

    pub fn emit_code_with_flag_imm(
        &mut self,
        code: Opcode,
        comma_ok: bool,
        imm: OpIndex,
        pos: Option<usize>,
    ) {
        let mut inst = Instruction::new(code, None, None, None, Some(imm));
        let flag = if comma_ok { 1 } else { 0 };
        inst.set_t2_with_index(flag);
        self.code.push(inst);
        self.pos.push(pos);
    }

    pub fn emit_code(&mut self, code: Opcode, pos: Option<usize>) {
        self.emit_inst(code, [None, None, None], None, pos);
    }

    /// returns the index of the const if it's found
    pub fn get_const_index(&self, val: &GosValue) -> Option<EntIndex> {
        self.consts.iter().enumerate().find_map(|(i, x)| {
            if val.identical(x) {
                Some(EntIndex::Const(i as OpIndex))
            } else {
                None
            }
        })
    }

    pub fn add_local(&mut self, entity: Option<KeyData>) -> EntIndex {
        let result = self.local_alloc;
        if let Some(key) = entity {
            let old = self.entities.insert(key, EntIndex::LocalVar(result));
            assert_eq!(old, None);
        };
        self.local_alloc += 1;
        EntIndex::LocalVar(result)
    }

    pub fn add_local_zero(&mut self, zero: GosValue, typ: ValueType) {
        self.stack_temp_types.push(typ);
        self.local_zeros.push(zero)
    }

    /// add a const or get the index of a const.
    /// when 'entity' is no none, it's a const define, so it should not be called with the
    /// same 'entity' more than once
    pub fn add_const(&mut self, entity: Option<KeyData>, cst: GosValue) -> EntIndex {
        if let Some(index) = self.get_const_index(&cst) {
            index
        } else {
            self.consts.push(cst);
            let result = (self.consts.len() - 1).try_into().unwrap();
            if let Some(key) = entity {
                let old = self.entities.insert(key, EntIndex::Const(result));
                assert_eq!(old, None);
            }
            EntIndex::Const(result)
        }
    }

    pub fn try_add_upvalue(&mut self, entity: &KeyData, uv: ValueDesc) -> EntIndex {
        match self.uv_entities.get(entity) {
            Some(i) => *i,
            None => self.add_upvalue(entity, uv),
        }
    }

    fn add_upvalue(&mut self, entity: &KeyData, uv: ValueDesc) -> EntIndex {
        self.up_ptrs.push(uv);
        let i = (self.up_ptrs.len() - 1).try_into().unwrap();
        let et = EntIndex::UpValue(i);
        self.uv_entities.insert(*entity, et);
        et
    }
}
