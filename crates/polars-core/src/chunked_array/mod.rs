//! The typed heart of every Series column.
#![allow(unsafe_op_in_unsafe_fn)]
use std::iter::Map;
use std::sync::Arc;

use arrow::array::*;
use arrow::bitmap::Bitmap;
use arrow::compute::concatenate::concatenate_unchecked;
use polars_compute::filter::filter_with_bitmap;

use crate::prelude::*;

pub mod ops;
#[macro_use]
pub mod arithmetic;
pub mod builder;
pub mod cast;
pub mod collect;
pub mod comparison;
pub mod flags;
pub mod float;
pub mod iterator;
#[cfg(feature = "ndarray")]
pub(crate) mod ndarray;

#[cfg(feature = "dtype-array")]
pub(crate) mod array;
mod binary;
mod bitwise;
#[cfg(feature = "object")]
mod drop;
mod from;
mod from_iterator;
pub mod from_iterator_par;
pub(crate) mod list;
pub(crate) mod logical;
#[cfg(feature = "object")]
pub mod object;
#[cfg(feature = "random")]
mod random;
#[cfg(feature = "dtype-struct")]
mod struct_;
#[cfg(any(
    feature = "temporal",
    feature = "dtype-datetime",
    feature = "dtype-date"
))]
pub mod temporal;
mod to_vec;
mod trusted_len;

use std::slice::Iter;

use arrow::legacy::prelude::*;
#[cfg(feature = "dtype-struct")]
pub use struct_::StructChunked;

use self::flags::{StatisticsFlags, StatisticsFlagsIM};
use crate::series::IsSorted;
use crate::utils::{first_non_null, last_non_null};

#[cfg(not(feature = "dtype-categorical"))]
pub struct RevMapping {}

pub type ChunkLenIter<'a> = std::iter::Map<std::slice::Iter<'a, ArrayRef>, fn(&ArrayRef) -> usize>;

/// # ChunkedArray
///
/// Every Series contains a [`ChunkedArray<T>`]. Unlike [`Series`], [`ChunkedArray`]s are typed. This allows
/// us to apply closures to the data and collect the results to a [`ChunkedArray`] of the same type `T`.
/// Below we use an apply to use the cosine function to the values of a [`ChunkedArray`].
///
/// ```rust
/// # use polars_core::prelude::*;
/// fn apply_cosine_and_cast(ca: &Float32Chunked) -> Float32Chunked {
///     ca.apply_values(|v| v.cos())
/// }
/// ```
///
/// ## Conversion between Series and ChunkedArrays
/// Conversion from a [`Series`] to a [`ChunkedArray`] is effortless.
///
/// ```rust
/// # use polars_core::prelude::*;
/// fn to_chunked_array(series: &Series) -> PolarsResult<&Int32Chunked>{
///     series.i32()
/// }
///
/// fn to_series(ca: Int32Chunked) -> Series {
///     ca.into_series()
/// }
/// ```
///
/// # Iterators
///
/// [`ChunkedArray`]s fully support Rust native [Iterator](https://doc.rust-lang.org/std/iter/trait.Iterator.html)
/// and [DoubleEndedIterator](https://doc.rust-lang.org/std/iter/trait.DoubleEndedIterator.html) traits, thereby
/// giving access to all the excellent methods available for [Iterators](https://doc.rust-lang.org/std/iter/trait.Iterator.html).
///
/// ```rust
/// # use polars_core::prelude::*;
///
/// fn iter_forward(ca: &Float32Chunked) {
///     ca.iter()
///         .for_each(|opt_v| println!("{:?}", opt_v))
/// }
///
/// fn iter_backward(ca: &Float32Chunked) {
///     ca.iter()
///         .rev()
///         .for_each(|opt_v| println!("{:?}", opt_v))
/// }
/// ```
///
/// # Memory layout
///
/// [`ChunkedArray`]s use [Apache Arrow](https://github.com/apache/arrow) as backend for the memory layout.
/// Arrows memory is immutable which makes it possible to make multiple zero copy (sub)-views from a single array.
///
/// To be able to append data, Polars uses chunks to append new memory locations, hence the [`ChunkedArray<T>`] data structure.
/// Appends are cheap, because it will not lead to a full reallocation of the whole array (as could be the case with a Rust Vec).
///
/// However, multiple chunks in a [`ChunkedArray`] will slow down many operations that need random access because we have an extra indirection
/// and indexes need to be mapped to the proper chunk. Arithmetic may also be slowed down by this.
/// When multiplying two [`ChunkedArray`]s with different chunk sizes they cannot utilize [SIMD](https://en.wikipedia.org/wiki/SIMD) for instance.
///
/// If you want to have predictable performance
/// (no unexpected re-allocation of memory), it is advised to call the [`ChunkedArray::rechunk`] after
/// multiple append operations.
///
/// See also [`ChunkedArray::extend`] for appends within a chunk.
///
/// # Invariants
/// - A [`ChunkedArray`] should always have at least a single [`ArrayRef`].
/// - The [`PolarsDataType`] `T` should always map to the correct [`ArrowDataType`] in the [`ArrayRef`]
///   chunks.
/// - Nested datatypes such as [`List`] and [`Array`] store the physical types instead of the
///   logical type given by the datatype.
///
/// [`List`]: crate::datatypes::DataType::List
pub struct ChunkedArray<T: PolarsDataType> {
    pub(crate) field: Arc<Field>,
    pub(crate) chunks: Vec<ArrayRef>,

    pub(crate) flags: StatisticsFlagsIM,

    length: usize,
    null_count: usize,
    _pd: std::marker::PhantomData<T>,
}

impl<T: PolarsDataType> ChunkedArray<T> {
    fn should_rechunk(&self) -> bool {
        self.chunks.len() > 1 && self.chunks.len() > self.len() / 3
    }

    fn optional_rechunk(mut self) -> Self {
        // Rechunk if we have many small chunks.
        if self.should_rechunk() {
            self.rechunk_mut()
        }
        self
    }

    pub(crate) fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Series to [`ChunkedArray<T>`]
    pub fn unpack_series_matching_type<'a>(
        &self,
        series: &'a Series,
    ) -> PolarsResult<&'a ChunkedArray<T>> {
        match self.dtype() {
            #[cfg(feature = "dtype-decimal")]
            DataType::Decimal(_, _) => {
                let logical = series.decimal()?;

                let ca = logical.physical();
                Ok(ca.as_any().downcast_ref::<ChunkedArray<T>>().unwrap())
            },
            dt => {
                polars_ensure!(
                    dt == series.dtype(),
                    SchemaMismatch: "cannot unpack series of type `{}` into `{}`",
                    series.dtype(),
                    dt,
                );

                // SAFETY:
                // dtype will be correct.
                Ok(unsafe { self.unpack_series_matching_physical_type(series) })
            },
        }
    }

    /// Create a new [`ChunkedArray`] and compute its `length` and `null_count`.
    ///
    /// If you want to explicitly the `length` and `null_count`, look at
    /// [`ChunkedArray::new_with_dims`]
    fn new_with_compute_len(field: Arc<Field>, chunks: Vec<ArrayRef>) -> Self {
        unsafe {
            let mut chunked_arr = Self::new_with_dims(field, chunks, 0, 0);
            chunked_arr.compute_len();
            chunked_arr
        }
    }

    /// Create a new [`ChunkedArray`] and explicitly set its `length` and `null_count`.
    /// # Safety
    /// The length and null_count must be correct.
    pub unsafe fn new_with_dims(
        field: Arc<Field>,
        chunks: Vec<ArrayRef>,
        length: usize,
        null_count: usize,
    ) -> Self {
        Self {
            field,
            chunks,
            flags: StatisticsFlagsIM::empty(),

            _pd: Default::default(),
            length,
            null_count,
        }
    }

    pub(crate) fn is_sorted_ascending_flag(&self) -> bool {
        self.get_flags().is_sorted_ascending()
    }

    pub(crate) fn is_sorted_descending_flag(&self) -> bool {
        self.get_flags().is_sorted_descending()
    }

    /// Whether `self` is sorted in any direction.
    pub(crate) fn is_sorted_any(&self) -> bool {
        self.get_flags().is_sorted_any()
    }

    pub fn unset_fast_explode_list(&mut self) {
        self.set_fast_explode_list(false)
    }

    pub fn set_fast_explode_list(&mut self, value: bool) {
        let mut flags = self.flags.get_mut();
        flags.set(StatisticsFlags::CAN_FAST_EXPLODE_LIST, value);
        self.flags.set_mut(flags);
    }

    pub fn get_fast_explode_list(&self) -> bool {
        self.get_flags().can_fast_explode_list()
    }

    pub fn get_flags(&self) -> StatisticsFlags {
        self.flags.get()
    }

    /// Set flags for the [`ChunkedArray`]
    pub fn set_flags(&mut self, flags: StatisticsFlags) {
        self.flags = StatisticsFlagsIM::new(flags);
    }

    pub fn is_sorted_flag(&self) -> IsSorted {
        self.get_flags().is_sorted()
    }

    pub fn retain_flags_from<U: PolarsDataType>(
        &mut self,
        from: &ChunkedArray<U>,
        retain_flags: StatisticsFlags,
    ) {
        let flags = from.flags.get();
        // Try to avoid write contention.
        if !flags.is_empty() {
            self.set_flags(flags & retain_flags)
        }
    }

    /// Set the 'sorted' bit meta info.
    pub fn set_sorted_flag(&mut self, sorted: IsSorted) {
        let mut flags = self.flags.get_mut();
        flags.set_sorted(sorted);
        self.flags.set_mut(flags);
    }

    /// Set the 'sorted' bit meta info.
    pub fn with_sorted_flag(&self, sorted: IsSorted) -> Self {
        let mut out = self.clone();
        out.set_sorted_flag(sorted);
        out
    }

    /// Get the index of the first non null value in this [`ChunkedArray`].
    pub fn first_non_null(&self) -> Option<usize> {
        if self.null_count() == self.len() {
            None
        }
        // We now know there is at least 1 non-null item in the array, and self.len() > 0
        else if self.null_count() == 0 {
            Some(0)
        } else if self.is_sorted_any() {
            let out = if unsafe { self.downcast_get_unchecked(0).is_null_unchecked(0) } {
                // nulls are all at the start
                self.null_count()
            } else {
                // nulls are all at the end
                0
            };

            debug_assert!(
                // If we are lucky this catches something.
                unsafe { self.get_unchecked(out) }.is_some(),
                "incorrect sorted flag"
            );

            Some(out)
        } else {
            first_non_null(self.iter_validities())
        }
    }

    /// Get the index of the last non null value in this [`ChunkedArray`].
    pub fn last_non_null(&self) -> Option<usize> {
        if self.null_count() == self.len() {
            None
        }
        // We now know there is at least 1 non-null item in the array, and self.len() > 0
        else if self.null_count() == 0 {
            Some(self.len() - 1)
        } else if self.is_sorted_any() {
            let out = if unsafe { self.downcast_get_unchecked(0).is_null_unchecked(0) } {
                // nulls are all at the start
                self.len() - 1
            } else {
                // nulls are all at the end
                self.len() - self.null_count() - 1
            };

            debug_assert!(
                // If we are lucky this catches something.
                unsafe { self.get_unchecked(out) }.is_some(),
                "incorrect sorted flag"
            );

            Some(out)
        } else {
            last_non_null(self.iter_validities(), self.len())
        }
    }

    pub fn drop_nulls(&self) -> Self {
        if self.null_count() == 0 {
            self.clone()
        } else {
            let chunks = self
                .downcast_iter()
                .map(|arr| {
                    if arr.null_count() == 0 {
                        arr.to_boxed()
                    } else {
                        filter_with_bitmap(arr, arr.validity().unwrap())
                    }
                })
                .collect();
            unsafe {
                Self::new_with_dims(
                    self.field.clone(),
                    chunks,
                    self.len() - self.null_count(),
                    0,
                )
            }
        }
    }

    /// Get the buffer of bits representing null values
    #[inline]
    #[allow(clippy::type_complexity)]
    pub fn iter_validities(&self) -> Map<Iter<'_, ArrayRef>, fn(&ArrayRef) -> Option<&Bitmap>> {
        fn to_validity(arr: &ArrayRef) -> Option<&Bitmap> {
            arr.validity()
        }
        self.chunks.iter().map(to_validity)
    }

    #[inline]
    /// Return if any the chunks in this [`ChunkedArray`] have nulls.
    pub fn has_nulls(&self) -> bool {
        self.null_count > 0
    }

    /// Shrink the capacity of this array to fit its length.
    pub fn shrink_to_fit(&mut self) {
        self.chunks = vec![concatenate_unchecked(self.chunks.as_slice()).unwrap()];
    }

    pub fn clear(&self) -> Self {
        // SAFETY: we keep the correct dtype
        let mut ca = unsafe {
            self.copy_with_chunks(vec![new_empty_array(
                self.chunks.first().unwrap().dtype().clone(),
            )])
        };

        use StatisticsFlags as F;
        ca.retain_flags_from(self, F::IS_SORTED_ANY | F::CAN_FAST_EXPLODE_LIST);
        ca
    }

    /// Unpack a [`Series`] to the same physical type.
    ///
    /// # Safety
    ///
    /// This is unsafe as the dtype may be incorrect and
    /// is assumed to be correct in other safe code.
    pub(crate) unsafe fn unpack_series_matching_physical_type<'a>(
        &self,
        series: &'a Series,
    ) -> &'a ChunkedArray<T> {
        let series_trait = &**series;
        if self.dtype() == series.dtype() {
            &*(series_trait as *const dyn SeriesTrait as *const ChunkedArray<T>)
        } else {
            use DataType::*;
            match (self.dtype(), series.dtype()) {
                (Int64, Datetime(_, _)) | (Int64, Duration(_)) | (Int32, Date) => {
                    &*(series_trait as *const dyn SeriesTrait as *const ChunkedArray<T>)
                },
                _ => panic!(
                    "cannot unpack series {:?} into matching type {:?}",
                    series,
                    self.dtype()
                ),
            }
        }
    }

    /// Returns an iterator over the lengths of the chunks of the array.
    pub fn chunk_lengths(&self) -> ChunkLenIter<'_> {
        self.chunks.iter().map(|chunk| chunk.len())
    }

    /// A reference to the chunks
    #[inline]
    pub fn chunks(&self) -> &Vec<ArrayRef> {
        &self.chunks
    }

    /// A mutable reference to the chunks
    ///
    /// # Safety
    /// The caller must ensure to not change the [`DataType`] or `length` of any of the chunks.
    /// And the `null_count` remains correct.
    #[inline]
    pub unsafe fn chunks_mut(&mut self) -> &mut Vec<ArrayRef> {
        &mut self.chunks
    }

    /// Returns true if contains a single chunk and has no null values
    pub fn is_optimal_aligned(&self) -> bool {
        self.chunks.len() == 1 && self.null_count() == 0
    }

    /// Create a new [`ChunkedArray`] from self, where the chunks are replaced.
    ///
    /// # Safety
    /// The caller must ensure the dtypes of the chunks are correct
    unsafe fn copy_with_chunks(&self, chunks: Vec<ArrayRef>) -> Self {
        Self::new_with_compute_len(self.field.clone(), chunks)
    }

    /// Get data type of [`ChunkedArray`].
    pub fn dtype(&self) -> &DataType {
        self.field.dtype()
    }

    pub(crate) unsafe fn set_dtype(&mut self, dtype: DataType) {
        self.field = Arc::new(Field::new(self.name().clone(), dtype))
    }

    /// Name of the [`ChunkedArray`].
    pub fn name(&self) -> &PlSmallStr {
        self.field.name()
    }

    /// Get a reference to the field.
    pub fn ref_field(&self) -> &Field {
        &self.field
    }

    /// Rename this [`ChunkedArray`].
    pub fn rename(&mut self, name: PlSmallStr) {
        self.field = Arc::new(Field::new(name, self.field.dtype().clone()));
    }

    /// Return this [`ChunkedArray`] with a new name.
    pub fn with_name(mut self, name: PlSmallStr) -> Self {
        self.rename(name);
        self
    }
}

impl<T> ChunkedArray<T>
where
    T: PolarsDataType,
{
    /// Get a single value from this [`ChunkedArray`]. If the return values is `None` this
    /// indicates a NULL value.
    ///
    /// # Panics
    /// This function will panic if `idx` is out of bounds.
    #[inline]
    pub fn get(&self, idx: usize) -> Option<T::Physical<'_>> {
        let (chunk_idx, arr_idx) = self.index_to_chunked_index(idx);
        assert!(
            chunk_idx < self.chunks().len(),
            "index: {} out of bounds for len: {}",
            idx,
            self.len()
        );
        unsafe {
            let arr = self.downcast_get_unchecked(chunk_idx);
            assert!(
                arr_idx < arr.len(),
                "index: {} out of bounds for len: {}",
                idx,
                self.len()
            );
            arr.get_unchecked(arr_idx)
        }
    }

    /// Get a single value from this [`ChunkedArray`]. If the return values is `None` this
    /// indicates a NULL value.
    ///
    /// # Safety
    /// It is the callers responsibility that the `idx < self.len()`.
    #[inline]
    pub unsafe fn get_unchecked(&self, idx: usize) -> Option<T::Physical<'_>> {
        let (chunk_idx, arr_idx) = self.index_to_chunked_index(idx);

        unsafe {
            // SAFETY: up to the caller to make sure the index is valid.
            self.downcast_get_unchecked(chunk_idx)
                .get_unchecked(arr_idx)
        }
    }

    /// Get a single value from this [`ChunkedArray`]. Null values are ignored and the returned
    /// value could be garbage if it was masked out by NULL. Note that the value always is initialized.
    ///
    /// # Safety
    /// It is the callers responsibility that the `idx < self.len()`.
    #[inline]
    pub unsafe fn value_unchecked(&self, idx: usize) -> T::Physical<'_> {
        let (chunk_idx, arr_idx) = self.index_to_chunked_index(idx);

        unsafe {
            // SAFETY: up to the caller to make sure the index is valid.
            self.downcast_get_unchecked(chunk_idx)
                .value_unchecked(arr_idx)
        }
    }

    #[inline]
    pub fn first(&self) -> Option<T::Physical<'_>> {
        unsafe {
            let arr = self.downcast_get_unchecked(0);
            arr.get_unchecked(0)
        }
    }

    #[inline]
    pub fn last(&self) -> Option<T::Physical<'_>> {
        unsafe {
            let arr = self.downcast_get_unchecked(self.chunks.len().checked_sub(1)?);
            arr.get_unchecked(arr.len().checked_sub(1)?)
        }
    }
}

impl ListChunked {
    #[inline]
    pub fn get_as_series(&self, idx: usize) -> Option<Series> {
        unsafe {
            Some(Series::from_chunks_and_dtype_unchecked(
                self.name().clone(),
                vec![self.get(idx)?],
                &self.inner_dtype().to_physical(),
            ))
        }
    }
}

#[cfg(feature = "dtype-array")]
impl ArrayChunked {
    #[inline]
    pub fn get_as_series(&self, idx: usize) -> Option<Series> {
        unsafe {
            Some(Series::from_chunks_and_dtype_unchecked(
                self.name().clone(),
                vec![self.get(idx)?],
                &self.inner_dtype().to_physical(),
            ))
        }
    }
}

impl<T> ChunkedArray<T>
where
    T: PolarsDataType,
{
    /// Should be used to match the chunk_id of another [`ChunkedArray`].
    /// # Panics
    /// It is the callers responsibility to ensure that this [`ChunkedArray`] has a single chunk.
    pub fn match_chunks<I>(&self, chunk_id: I) -> Self
    where
        I: Iterator<Item = usize>,
    {
        debug_assert!(self.chunks.len() == 1);
        // Takes a ChunkedArray containing a single chunk.
        let slice = |ca: &Self| {
            let array = &ca.chunks[0];

            let mut offset = 0;
            let chunks = chunk_id
                .map(|len| {
                    // SAFETY: within bounds.
                    debug_assert!((offset + len) <= array.len());
                    let out = unsafe { array.sliced_unchecked(offset, len) };
                    offset += len;
                    out
                })
                .collect();

            debug_assert_eq!(offset, array.len());

            // SAFETY: We just slice the original chunks, their type will not change.
            unsafe {
                Self::from_chunks_and_dtype(self.name().clone(), chunks, self.dtype().clone())
            }
        };

        if self.chunks.len() != 1 {
            let out = self.rechunk();
            slice(&out)
        } else {
            slice(self)
        }
    }
}

impl<T: PolarsDataType> AsRefDataType for ChunkedArray<T> {
    fn as_ref_dtype(&self) -> &DataType {
        self.dtype()
    }
}

pub(crate) trait AsSinglePtr: AsRefDataType {
    /// Rechunk and return a ptr to the start of the array
    fn as_single_ptr(&mut self) -> PolarsResult<usize> {
        polars_bail!(opq = as_single_ptr, self.as_ref_dtype());
    }
}

impl<T> AsSinglePtr for ChunkedArray<T>
where
    T: PolarsNumericType,
{
    fn as_single_ptr(&mut self) -> PolarsResult<usize> {
        self.rechunk_mut();
        let a = self.data_views().next().unwrap();
        let ptr = a.as_ptr();
        Ok(ptr as usize)
    }
}

impl AsSinglePtr for BooleanChunked {}
impl AsSinglePtr for ListChunked {}
#[cfg(feature = "dtype-array")]
impl AsSinglePtr for ArrayChunked {}
impl AsSinglePtr for StringChunked {}
impl AsSinglePtr for BinaryChunked {}
#[cfg(feature = "object")]
impl<T: PolarsObject> AsSinglePtr for ObjectChunked<T> {}

pub enum ChunkedArrayLayout<'a, T: PolarsDataType> {
    SingleNoNull(&'a T::Array),
    Single(&'a T::Array),
    MultiNoNull(&'a ChunkedArray<T>),
    Multi(&'a ChunkedArray<T>),
}

impl<T> ChunkedArray<T>
where
    T: PolarsDataType,
{
    pub fn layout(&self) -> ChunkedArrayLayout<'_, T> {
        if self.chunks.len() == 1 {
            let arr = self.downcast_iter().next().unwrap();
            return if arr.null_count() == 0 {
                ChunkedArrayLayout::SingleNoNull(arr)
            } else {
                ChunkedArrayLayout::Single(arr)
            };
        }

        if self.downcast_iter().all(|a| a.null_count() == 0) {
            ChunkedArrayLayout::MultiNoNull(self)
        } else {
            ChunkedArrayLayout::Multi(self)
        }
    }
}

impl<T> ChunkedArray<T>
where
    T: PolarsNumericType,
{
    /// Returns the values of the array as a contiguous slice.
    pub fn cont_slice(&self) -> PolarsResult<&[T::Native]> {
        polars_ensure!(
            self.chunks.len() == 1 && self.chunks[0].null_count() == 0,
            ComputeError: "chunked array is not contiguous"
        );
        Ok(self.downcast_iter().next().map(|arr| arr.values()).unwrap())
    }

    /// Returns the values of the array as a contiguous mutable slice.
    pub(crate) fn cont_slice_mut(&mut self) -> Option<&mut [T::Native]> {
        if self.chunks.len() == 1 && self.chunks[0].null_count() == 0 {
            // SAFETY, we will not swap the PrimitiveArray.
            let arr = unsafe { self.downcast_iter_mut().next().unwrap() };
            arr.get_mut_values()
        } else {
            None
        }
    }

    /// Get slices of the underlying arrow data.
    /// NOTE: null values should be taken into account by the user of these slices as they are handled
    /// separately
    pub fn data_views(&self) -> impl DoubleEndedIterator<Item = &[T::Native]> {
        self.downcast_iter().map(|arr| arr.values().as_slice())
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn into_no_null_iter(
        &self,
    ) -> impl '_ + Send + Sync + ExactSizeIterator<Item = T::Native> + DoubleEndedIterator + TrustedLen
    {
        // .copied was significantly slower in benchmark, next call did not inline?
        #[allow(clippy::map_clone)]
        // we know the iterators len
        unsafe {
            self.data_views()
                .flatten()
                .map(|v| *v)
                .trust_my_length(self.len())
        }
    }
}

impl<T: PolarsDataType> Clone for ChunkedArray<T> {
    fn clone(&self) -> Self {
        ChunkedArray {
            field: self.field.clone(),
            chunks: self.chunks.clone(),
            flags: self.flags.clone(),

            _pd: Default::default(),
            length: self.length,
            null_count: self.null_count,
        }
    }
}

impl<T: PolarsDataType> AsRef<ChunkedArray<T>> for ChunkedArray<T> {
    fn as_ref(&self) -> &ChunkedArray<T> {
        self
    }
}

impl ValueSize for ListChunked {
    fn get_values_size(&self) -> usize {
        self.chunks
            .iter()
            .fold(0usize, |acc, arr| acc + arr.get_values_size())
    }
}

#[cfg(feature = "dtype-array")]
impl ValueSize for ArrayChunked {
    fn get_values_size(&self) -> usize {
        self.chunks
            .iter()
            .fold(0usize, |acc, arr| acc + arr.get_values_size())
    }
}
impl ValueSize for StringChunked {
    fn get_values_size(&self) -> usize {
        self.chunks
            .iter()
            .fold(0usize, |acc, arr| acc + arr.get_values_size())
    }
}

impl ValueSize for BinaryOffsetChunked {
    fn get_values_size(&self) -> usize {
        self.chunks
            .iter()
            .fold(0usize, |acc, arr| acc + arr.get_values_size())
    }
}

pub(crate) fn to_primitive<T: PolarsNumericType>(
    values: Vec<T::Native>,
    validity: Option<Bitmap>,
) -> PrimitiveArray<T::Native> {
    PrimitiveArray::new(
        T::get_static_dtype().to_arrow(CompatLevel::newest()),
        values.into(),
        validity,
    )
}

pub(crate) fn to_array<T: PolarsNumericType>(
    values: Vec<T::Native>,
    validity: Option<Bitmap>,
) -> ArrayRef {
    Box::new(to_primitive::<T>(values, validity))
}

impl<T: PolarsDataType> Default for ChunkedArray<T> {
    fn default() -> Self {
        let dtype = T::get_static_dtype();
        let arrow_dtype = dtype.to_physical().to_arrow(CompatLevel::newest());
        ChunkedArray {
            field: Arc::new(Field::new(PlSmallStr::EMPTY, dtype)),
            // Invariant: always has 1 chunk.
            chunks: vec![new_empty_array(arrow_dtype)],
            flags: StatisticsFlagsIM::empty(),

            _pd: Default::default(),
            length: 0,
            null_count: 0,
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use crate::prelude::*;

    pub(crate) fn get_chunked_array() -> Int32Chunked {
        ChunkedArray::new(PlSmallStr::from_static("a"), &[1, 2, 3])
    }

    #[test]
    fn test_sort() {
        let a = Int32Chunked::new(PlSmallStr::from_static("a"), &[1, 9, 3, 2]);
        let b = a
            .sort(false)
            .into_iter()
            .map(|opt| opt.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(b, [1, 2, 3, 9]);
        let a = StringChunked::new(PlSmallStr::from_static("a"), &["b", "a", "c"]);
        let a = a.sort(false);
        let b = a.into_iter().collect::<Vec<_>>();
        assert_eq!(b, [Some("a"), Some("b"), Some("c")]);
        assert!(a.is_sorted_ascending_flag());
    }

    #[test]
    fn arithmetic() {
        let a = &Int32Chunked::new(PlSmallStr::from_static("a"), &[1, 100, 6, 40]);
        let b = &Int32Chunked::new(PlSmallStr::from_static("b"), &[-1, 2, 3, 4]);

        // Not really asserting anything here but still making sure the code is exercised
        // This (and more) is properly tested from the integration test suite and Python bindings.
        println!("{:?}", a + b);
        println!("{:?}", a - b);
        println!("{:?}", a * b);
        println!("{:?}", a / b);
    }

    #[test]
    fn iter() {
        let s1 = get_chunked_array();
        // sum
        assert_eq!(s1.into_iter().fold(0, |acc, val| { acc + val.unwrap() }), 6)
    }

    #[test]
    fn limit() {
        let a = get_chunked_array();
        let b = a.limit(2);
        println!("{b:?}");
        assert_eq!(b.len(), 2)
    }

    #[test]
    fn filter() {
        let a = get_chunked_array();
        let b = a
            .filter(&BooleanChunked::new(
                PlSmallStr::from_static("filter"),
                &[true, false, false],
            ))
            .unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.into_iter().next(), Some(Some(1)));
    }

    #[test]
    fn aggregates() {
        let a = &Int32Chunked::new(PlSmallStr::from_static("a"), &[1, 100, 10, 9]);
        assert_eq!(a.max(), Some(100));
        assert_eq!(a.min(), Some(1));
        assert_eq!(a.sum(), Some(120))
    }

    #[test]
    fn take() {
        let a = get_chunked_array();
        let new = a.take(&[0 as IdxSize, 1]).unwrap();
        assert_eq!(new.len(), 2)
    }

    #[test]
    fn cast() {
        let a = get_chunked_array();
        let b = a.cast(&DataType::Int64).unwrap();
        assert_eq!(b.dtype(), &DataType::Int64)
    }

    fn assert_slice_equal<T>(ca: &ChunkedArray<T>, eq: &[T::Native])
    where
        T: PolarsNumericType,
    {
        assert_eq!(ca.iter().map(|opt| opt.unwrap()).collect::<Vec<_>>(), eq)
    }

    #[test]
    fn slice() {
        let mut first = UInt32Chunked::new(PlSmallStr::from_static("first"), &[0, 1, 2]);
        let second = UInt32Chunked::new(PlSmallStr::from_static("second"), &[3, 4, 5]);
        first.append(&second).unwrap();
        assert_slice_equal(&first.slice(0, 3), &[0, 1, 2]);
        assert_slice_equal(&first.slice(0, 4), &[0, 1, 2, 3]);
        assert_slice_equal(&first.slice(1, 4), &[1, 2, 3, 4]);
        assert_slice_equal(&first.slice(3, 2), &[3, 4]);
        assert_slice_equal(&first.slice(3, 3), &[3, 4, 5]);
        assert_slice_equal(&first.slice(-3, 3), &[3, 4, 5]);
        assert_slice_equal(&first.slice(-6, 6), &[0, 1, 2, 3, 4, 5]);

        assert_eq!(first.slice(-7, 2).len(), 1);
        assert_eq!(first.slice(-3, 4).len(), 3);
        assert_eq!(first.slice(3, 4).len(), 3);
        assert_eq!(first.slice(10, 4).len(), 0);
    }

    #[test]
    fn sorting() {
        let s = UInt32Chunked::new(PlSmallStr::EMPTY, &[9, 2, 4]);
        let sorted = s.sort(false);
        assert_slice_equal(&sorted, &[2, 4, 9]);
        let sorted = s.sort(true);
        assert_slice_equal(&sorted, &[9, 4, 2]);

        let s: StringChunked = ["b", "a", "z"].iter().collect();
        let sorted = s.sort(false);
        assert_eq!(
            sorted.into_iter().collect::<Vec<_>>(),
            &[Some("a"), Some("b"), Some("z")]
        );
        let sorted = s.sort(true);
        assert_eq!(
            sorted.into_iter().collect::<Vec<_>>(),
            &[Some("z"), Some("b"), Some("a")]
        );
        let s: StringChunked = [Some("b"), None, Some("z")].iter().copied().collect();
        let sorted = s.sort(false);
        assert_eq!(
            sorted.into_iter().collect::<Vec<_>>(),
            &[None, Some("b"), Some("z")]
        );
    }

    #[test]
    fn reverse() {
        let s = UInt32Chunked::new(PlSmallStr::EMPTY, &[1, 2, 3]);
        // path with continuous slice
        assert_slice_equal(&s.reverse(), &[3, 2, 1]);
        // path with options
        let s = UInt32Chunked::new(PlSmallStr::EMPTY, &[Some(1), None, Some(3)]);
        assert_eq!(Vec::from(&s.reverse()), &[Some(3), None, Some(1)]);
        let s = BooleanChunked::new(PlSmallStr::EMPTY, &[true, false]);
        assert_eq!(Vec::from(&s.reverse()), &[Some(false), Some(true)]);

        let s = StringChunked::new(PlSmallStr::EMPTY, &["a", "b", "c"]);
        assert_eq!(Vec::from(&s.reverse()), &[Some("c"), Some("b"), Some("a")]);

        let s = StringChunked::new(PlSmallStr::EMPTY, &[Some("a"), None, Some("c")]);
        assert_eq!(Vec::from(&s.reverse()), &[Some("c"), None, Some("a")]);
    }

    #[test]
    #[cfg(feature = "dtype-categorical")]
    fn test_iter_categorical() {
        let ca = StringChunked::new(
            PlSmallStr::EMPTY,
            &[Some("foo"), None, Some("bar"), Some("ham")],
        );
        let cats = Categories::new(
            PlSmallStr::EMPTY,
            PlSmallStr::EMPTY,
            CategoricalPhysical::U32,
        );
        let ca = ca.cast(&DataType::from_categories(cats)).unwrap();
        let ca = ca.cat32().unwrap();
        let v: Vec<_> = ca.physical().into_iter().collect();
        assert_eq!(v, &[Some(0), None, Some(1), Some(2)]);
    }

    #[test]
    #[ignore]
    fn test_shrink_to_fit() {
        let mut builder = StringChunkedBuilder::new(PlSmallStr::from_static("foo"), 2048);
        builder.append_value("foo");
        let mut arr = builder.finish();
        let before = arr
            .chunks()
            .iter()
            .map(|arr| arrow::compute::aggregate::estimated_bytes_size(arr.as_ref()))
            .sum::<usize>();
        arr.shrink_to_fit();
        let after = arr
            .chunks()
            .iter()
            .map(|arr| arrow::compute::aggregate::estimated_bytes_size(arr.as_ref()))
            .sum::<usize>();
        assert!(before > after);
    }
}
