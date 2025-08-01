#![allow(unsafe_op_in_unsafe_fn)]
use std::ops::Deref;
use std::sync::LazyLock;

use either::Either;
use polars_error::{PolarsResult, polars_bail};
use polars_utils::relaxed_cell::RelaxedCell;

use super::utils::{self, BitChunk, BitChunks, BitmapIter, count_zeros, fmt, get_bit_unchecked};
use super::{IntoIter, MutableBitmap, chunk_iter_to_vec, num_intersections_with};
use crate::array::Splitable;
use crate::bitmap::aligned::AlignedBitmapSlice;
use crate::bitmap::iterator::{
    FastU32BitmapIter, FastU56BitmapIter, FastU64BitmapIter, TrueIdxIter,
};
use crate::bitmap::utils::bytes_for;
use crate::legacy::utils::FromTrustedLenIterator;
use crate::storage::SharedStorage;
use crate::trusted_len::TrustedLen;

const UNKNOWN_BIT_COUNT: u64 = u64::MAX;

/// An immutable container semantically equivalent to `Arc<Vec<bool>>` but represented as `Arc<Vec<u8>>` where
/// each boolean is represented as a single bit.
///
/// # Examples
/// ```
/// use polars_arrow::bitmap::{Bitmap, MutableBitmap};
///
/// let bitmap = Bitmap::from([true, false, true]);
/// assert_eq!(bitmap.iter().collect::<Vec<_>>(), vec![true, false, true]);
///
/// // creation directly from bytes
/// let bitmap = Bitmap::try_new(vec![0b00001101], 5).unwrap();
/// // note: the first bit is the left-most of the first byte
/// assert_eq!(bitmap.iter().collect::<Vec<_>>(), vec![true, false, true, true, false]);
/// // we can also get the slice:
/// assert_eq!(bitmap.as_slice(), ([0b00001101u8].as_ref(), 0, 5));
/// // debug helps :)
/// assert_eq!(format!("{:?}", bitmap), "Bitmap { len: 5, offset: 0, bytes: [0b___01101] }");
///
/// // it supports copy-on-write semantics (to a `MutableBitmap`)
/// let bitmap: MutableBitmap = bitmap.into_mut().right().unwrap();
/// assert_eq!(bitmap, MutableBitmap::from([true, false, true, true, false]));
///
/// // slicing is 'O(1)' (data is shared)
/// let bitmap = Bitmap::try_new(vec![0b00001101], 5).unwrap();
/// let mut sliced = bitmap.clone();
/// sliced.slice(1, 4);
/// assert_eq!(sliced.as_slice(), ([0b00001101u8].as_ref(), 1, 4)); // 1 here is the offset:
/// assert_eq!(format!("{:?}", sliced), "Bitmap { len: 4, offset: 1, bytes: [0b___0110_] }");
/// // when sliced (or cloned), it is no longer possible to `into_mut`.
/// let same: Bitmap = sliced.into_mut().left().unwrap();
/// ```
#[derive(Default, Clone)]
pub struct Bitmap {
    storage: SharedStorage<u8>,
    // Both offset and length are measured in bits. They are used to bound the
    // bitmap to a region of Bytes.
    offset: usize,
    length: usize,

    // A bit field that contains our cache for the number of unset bits.
    // If it is u64::MAX, we have no known value at all.
    // Other bit patterns where the top bit is set is reserved for future use.
    // If the top bit is not set we have an exact count.
    unset_bit_count_cache: RelaxedCell<u64>,
}

#[inline(always)]
fn has_cached_unset_bit_count(ubcc: u64) -> bool {
    ubcc >> 63 == 0
}

impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (bytes, offset, len) = self.as_slice();
        fmt(bytes, offset, len, f)
    }
}

pub(super) fn check(bytes: &[u8], offset: usize, length: usize) -> PolarsResult<()> {
    if offset + length > bytes.len().saturating_mul(8) {
        polars_bail!(InvalidOperation:
            "The offset + length of the bitmap ({}) must be `<=` to the number of bytes times 8 ({})",
            offset + length,
            bytes.len().saturating_mul(8)
        );
    }
    Ok(())
}

impl Bitmap {
    /// Initializes an empty [`Bitmap`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Initializes a new [`Bitmap`] from vector of bytes and a length.
    /// # Errors
    /// This function errors iff `length > bytes.len() * 8`
    #[inline]
    pub fn try_new(bytes: Vec<u8>, length: usize) -> PolarsResult<Self> {
        check(&bytes, 0, length)?;
        Ok(Self {
            storage: SharedStorage::from_vec(bytes),
            length,
            offset: 0,
            unset_bit_count_cache: RelaxedCell::from(if length == 0 {
                0
            } else {
                UNKNOWN_BIT_COUNT
            }),
        })
    }

    /// Returns the length of the [`Bitmap`].
    #[inline]
    pub fn len(&self) -> usize {
        self.length
    }

    /// Returns whether [`Bitmap`] is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a new iterator of `bool` over this bitmap
    pub fn iter(&self) -> BitmapIter<'_> {
        BitmapIter::new(&self.storage, self.offset, self.length)
    }

    /// Returns an iterator over bits in bit chunks [`BitChunk`].
    ///
    /// This iterator is useful to operate over multiple bits via e.g. bitwise.
    pub fn chunks<T: BitChunk>(&self) -> BitChunks<'_, T> {
        BitChunks::new(&self.storage, self.offset, self.length)
    }

    /// Returns a fast iterator that gives 32 bits at a time.
    /// Has a remainder that must be handled separately.
    pub fn fast_iter_u32(&self) -> FastU32BitmapIter<'_> {
        FastU32BitmapIter::new(&self.storage, self.offset, self.length)
    }

    /// Returns a fast iterator that gives 56 bits at a time.
    /// Has a remainder that must be handled separately.
    pub fn fast_iter_u56(&self) -> FastU56BitmapIter<'_> {
        FastU56BitmapIter::new(&self.storage, self.offset, self.length)
    }

    /// Returns a fast iterator that gives 64 bits at a time.
    /// Has a remainder that must be handled separately.
    pub fn fast_iter_u64(&self) -> FastU64BitmapIter<'_> {
        FastU64BitmapIter::new(&self.storage, self.offset, self.length)
    }

    /// Returns an iterator that only iterates over the set bits.
    pub fn true_idx_iter(&self) -> TrueIdxIter<'_> {
        TrueIdxIter::new(self.len(), Some(self))
    }

    /// Returns the bits of this [`Bitmap`] as a [`AlignedBitmapSlice`].
    pub fn aligned<T: BitChunk>(&self) -> AlignedBitmapSlice<'_, T> {
        AlignedBitmapSlice::new(&self.storage, self.offset, self.length)
    }

    /// Returns the byte slice of this [`Bitmap`].
    ///
    /// The returned tuple contains:
    /// * `.1`: The byte slice, truncated to the start of the first bit. So the start of the slice
    ///   is within the first 8 bits.
    /// * `.2`: The start offset in bits on a range `0 <= offsets < 8`.
    /// * `.3`: The length in number of bits.
    #[inline]
    pub fn as_slice(&self) -> (&[u8], usize, usize) {
        let start = self.offset / 8;
        let len = (self.offset % 8 + self.length).saturating_add(7) / 8;
        (
            &self.storage[start..start + len],
            self.offset % 8,
            self.length,
        )
    }

    /// Returns the number of set bits on this [`Bitmap`].
    ///
    /// See `unset_bits` for details.
    #[inline]
    pub fn set_bits(&self) -> usize {
        self.length - self.unset_bits()
    }

    /// Returns the number of set bits on this [`Bitmap`] if it is known.
    ///
    /// See `lazy_unset_bits` for details.
    #[inline]
    pub fn lazy_set_bits(&self) -> Option<usize> {
        Some(self.length - self.lazy_unset_bits()?)
    }

    /// Returns the number of unset bits on this [`Bitmap`].
    ///
    /// Guaranteed to be `<= self.len()`.
    ///
    /// # Implementation
    ///
    /// This function counts the number of unset bits if it is not already
    /// computed. Repeated calls use the cached bitcount.
    pub fn unset_bits(&self) -> usize {
        self.lazy_unset_bits().unwrap_or_else(|| {
            let zeros = count_zeros(&self.storage, self.offset, self.length);
            self.unset_bit_count_cache.store(zeros as u64);
            zeros
        })
    }

    /// Returns the number of unset bits on this [`Bitmap`] if it is known.
    ///
    /// Guaranteed to be `<= self.len()`.
    pub fn lazy_unset_bits(&self) -> Option<usize> {
        let cache = self.unset_bit_count_cache.load();
        has_cached_unset_bit_count(cache).then_some(cache as usize)
    }

    /// Updates the count of the number of set bits on this [`Bitmap`].
    ///
    /// # Safety
    ///
    /// The number of set bits must be correct.
    pub unsafe fn update_bit_count(&mut self, bits_set: usize) {
        assert!(bits_set <= self.length);
        let zeros = self.length - bits_set;
        self.unset_bit_count_cache.store(zeros as u64);
    }

    /// Slices `self`, offsetting by `offset` and truncating up to `length` bits.
    /// # Panic
    /// Panics iff `offset + length > self.length`, i.e. if the offset and `length`
    /// exceeds the allocated capacity of `self`.
    #[inline]
    pub fn slice(&mut self, offset: usize, length: usize) {
        assert!(offset + length <= self.length);
        unsafe { self.slice_unchecked(offset, length) }
    }

    /// Slices `self`, offsetting by `offset` and truncating up to `length` bits.
    ///
    /// # Safety
    /// The caller must ensure that `self.offset + offset + length <= self.len()`
    #[inline]
    pub unsafe fn slice_unchecked(&mut self, offset: usize, length: usize) {
        // Fast path: no-op slice.
        if offset == 0 && length == self.length {
            return;
        }

        // Fast path: we have no nulls or are full-null.
        let unset_bit_count_cache = self.unset_bit_count_cache.get_mut();
        if *unset_bit_count_cache == 0 || *unset_bit_count_cache == self.length as u64 {
            let new_count = if *unset_bit_count_cache > 0 {
                length as u64
            } else {
                0
            };
            *unset_bit_count_cache = new_count;
            self.offset += offset;
            self.length = length;
            return;
        }

        if has_cached_unset_bit_count(*unset_bit_count_cache) {
            // If we keep all but a small portion of the array it is worth
            // doing an eager re-count since we can reuse the old count via the
            // inclusion-exclusion principle.
            let small_portion = (self.length / 5).max(32);
            if length + small_portion >= self.length {
                // Subtract the null count of the chunks we slice off.
                let slice_end = self.offset + offset + length;
                let head_count = count_zeros(&self.storage, self.offset, offset);
                let tail_count =
                    count_zeros(&self.storage, slice_end, self.length - length - offset);
                let new_count = *unset_bit_count_cache - head_count as u64 - tail_count as u64;
                *unset_bit_count_cache = new_count;
            } else {
                *unset_bit_count_cache = UNKNOWN_BIT_COUNT;
            }
        }

        self.offset += offset;
        self.length = length;
    }

    /// Slices `self`, offsetting by `offset` and truncating up to `length` bits.
    /// # Panic
    /// Panics iff `offset + length > self.length`, i.e. if the offset and `length`
    /// exceeds the allocated capacity of `self`.
    #[inline]
    #[must_use]
    pub fn sliced(self, offset: usize, length: usize) -> Self {
        assert!(offset + length <= self.length);
        unsafe { self.sliced_unchecked(offset, length) }
    }

    /// Slices `self`, offsetting by `offset` and truncating up to `length` bits.
    ///
    /// # Safety
    /// The caller must ensure that `self.offset + offset + length <= self.len()`
    #[inline]
    #[must_use]
    pub unsafe fn sliced_unchecked(mut self, offset: usize, length: usize) -> Self {
        self.slice_unchecked(offset, length);
        self
    }

    /// Returns whether the bit at position `i` is set.
    /// # Panics
    /// Panics iff `i >= self.len()`.
    #[inline]
    pub fn get_bit(&self, i: usize) -> bool {
        assert!(i < self.len());
        unsafe { self.get_bit_unchecked(i) }
    }

    /// Unsafely returns whether the bit at position `i` is set.
    ///
    /// # Safety
    /// Unsound iff `i >= self.len()`.
    #[inline]
    pub unsafe fn get_bit_unchecked(&self, i: usize) -> bool {
        debug_assert!(i < self.len());
        get_bit_unchecked(&self.storage, self.offset + i)
    }

    /// Returns a pointer to the start of this [`Bitmap`] (ignores `offsets`)
    /// This pointer is allocated iff `self.len() > 0`.
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.storage.deref().as_ptr()
    }

    /// Returns a pointer to the start of this [`Bitmap`] (ignores `offsets`)
    /// This pointer is allocated iff `self.len() > 0`.
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    /// Converts this [`Bitmap`] to [`MutableBitmap`], returning itself if the conversion
    /// is not possible
    ///
    /// This operation returns a [`MutableBitmap`] iff:
    /// * this [`Bitmap`] is not an offsetted slice of another [`Bitmap`]
    /// * this [`Bitmap`] has not been cloned (i.e. [`Arc`]`::get_mut` yields [`Some`])
    /// * this [`Bitmap`] was not imported from the c data interface (FFI)
    pub fn into_mut(mut self) -> Either<Self, MutableBitmap> {
        match self.storage.try_into_vec() {
            Ok(v) => Either::Right(MutableBitmap::from_vec(v, self.length)),
            Err(storage) => {
                self.storage = storage;
                Either::Left(self)
            },
        }
    }

    /// Converts this [`Bitmap`] into a [`MutableBitmap`], cloning its internal
    /// buffer if required (clone-on-write).
    pub fn make_mut(self) -> MutableBitmap {
        match self.into_mut() {
            Either::Left(data) => {
                if data.offset > 0 {
                    // re-align the bits (remove the offset)
                    let chunks = data.chunks::<u64>();
                    let remainder = chunks.remainder();
                    let vec = chunk_iter_to_vec(chunks.chain(std::iter::once(remainder)));
                    MutableBitmap::from_vec(vec, data.length)
                } else {
                    let len = bytes_for(data.length);
                    MutableBitmap::from_vec(data.storage[0..len].to_vec(), data.length)
                }
            },
            Either::Right(data) => data,
        }
    }

    /// Initializes an new [`Bitmap`] filled with unset values.
    #[inline]
    pub fn new_zeroed(length: usize) -> Self {
        // We intentionally leak 1MiB of zeroed memory once so we don't have to
        // refcount it.
        const GLOBAL_ZERO_SIZE: usize = 1024 * 1024;
        static GLOBAL_ZEROES: LazyLock<SharedStorage<u8>> = LazyLock::new(|| {
            let mut ss = SharedStorage::from_vec(vec![0; GLOBAL_ZERO_SIZE]);
            ss.leak();
            ss
        });

        let bytes_needed = length.div_ceil(8);
        let storage = if bytes_needed <= GLOBAL_ZERO_SIZE {
            GLOBAL_ZEROES.clone()
        } else {
            SharedStorage::from_vec(vec![0; bytes_needed])
        };
        Self {
            storage,
            offset: 0,
            length,
            unset_bit_count_cache: RelaxedCell::from(length as u64),
        }
    }

    /// Initializes an new [`Bitmap`] filled with the given value.
    #[inline]
    pub fn new_with_value(value: bool, length: usize) -> Self {
        if !value {
            return Self::new_zeroed(length);
        }

        unsafe {
            Bitmap::from_inner_unchecked(
                SharedStorage::from_vec(vec![u8::MAX; length.saturating_add(7) / 8]),
                0,
                length,
                Some(0),
            )
        }
    }

    /// Counts the nulls (unset bits) starting from `offset` bits and for `length` bits.
    #[inline]
    pub fn null_count_range(&self, offset: usize, length: usize) -> usize {
        count_zeros(&self.storage, self.offset + offset, length)
    }

    /// Creates a new [`Bitmap`] from a slice and length.
    /// # Panic
    /// Panics iff `length > bytes.len() * 8`
    #[inline]
    pub fn from_u8_slice<T: AsRef<[u8]>>(slice: T, length: usize) -> Self {
        Bitmap::try_new(slice.as_ref().to_vec(), length).unwrap()
    }

    /// Alias for `Bitmap::try_new().unwrap()`
    /// This function is `O(1)`
    /// # Panic
    /// This function panics iff `length > bytes.len() * 8`
    #[inline]
    pub fn from_u8_vec(vec: Vec<u8>, length: usize) -> Self {
        Bitmap::try_new(vec, length).unwrap()
    }

    /// Returns whether the bit at position `i` is set.
    #[inline]
    pub fn get(&self, i: usize) -> Option<bool> {
        if i < self.len() {
            Some(unsafe { self.get_bit_unchecked(i) })
        } else {
            None
        }
    }

    /// Creates a [`Bitmap`] from its internal representation.
    /// This is the inverted from [`Bitmap::into_inner`]
    ///
    /// # Safety
    /// Callers must ensure all invariants of this struct are upheld.
    pub unsafe fn from_inner_unchecked(
        storage: SharedStorage<u8>,
        offset: usize,
        length: usize,
        unset_bits: Option<usize>,
    ) -> Self {
        debug_assert!(check(&storage[..], offset, length).is_ok());

        let unset_bit_count_cache = if let Some(n) = unset_bits {
            RelaxedCell::from(n as u64)
        } else {
            RelaxedCell::from(UNKNOWN_BIT_COUNT)
        };
        Self {
            storage,
            offset,
            length,
            unset_bit_count_cache,
        }
    }

    /// Checks whether two [`Bitmap`]s have shared set bits.
    ///
    /// This is an optimized version of `(self & other) != 0000..`.
    pub fn intersects_with(&self, other: &Self) -> bool {
        self.num_intersections_with(other) != 0
    }

    /// Calculates the number of shared set bits between two [`Bitmap`]s.
    pub fn num_intersections_with(&self, other: &Self) -> usize {
        num_intersections_with(self, other)
    }

    /// Select between `truthy` and `falsy` based on `self`.
    ///
    /// This essentially performs:
    ///
    /// `out[i] = if self[i] { truthy[i] } else { falsy[i] }`
    pub fn select(&self, truthy: &Self, falsy: &Self) -> Self {
        super::bitmap_ops::select(self, truthy, falsy)
    }

    /// Select between `truthy` and constant `falsy` based on `self`.
    ///
    /// This essentially performs:
    ///
    /// `out[i] = if self[i] { truthy[i] } else { falsy }`
    pub fn select_constant(&self, truthy: &Self, falsy: bool) -> Self {
        super::bitmap_ops::select_constant(self, truthy, falsy)
    }

    /// Calculates the number of edges from `0 -> 1` and `1 -> 0`.
    pub fn num_edges(&self) -> usize {
        super::bitmap_ops::num_edges(self)
    }

    /// Returns the number of zero bits from the start before a one bit is seen
    pub fn leading_zeros(&self) -> usize {
        utils::leading_zeros(&self.storage, self.offset, self.length)
    }
    /// Returns the number of one bits from the start before a zero bit is seen
    pub fn leading_ones(&self) -> usize {
        utils::leading_ones(&self.storage, self.offset, self.length)
    }
    /// Returns the number of zero bits from the back before a one bit is seen
    pub fn trailing_zeros(&self) -> usize {
        utils::trailing_zeros(&self.storage, self.offset, self.length)
    }
    /// Returns the number of one bits from the back before a zero bit is seen
    pub fn trailing_ones(&self) -> usize {
        utils::trailing_ones(&self.storage, self.offset, self.length)
    }

    /// Take all `0` bits at the start of the [`Bitmap`] before a `1` is seen, returning how many
    /// bits were taken
    pub fn take_leading_zeros(&mut self) -> usize {
        if self
            .lazy_unset_bits()
            .is_some_and(|unset_bits| unset_bits == self.length)
        {
            let leading_zeros = self.length;
            self.offset += self.length;
            self.length = 0;
            *self.unset_bit_count_cache.get_mut() = 0;
            return leading_zeros;
        }

        let leading_zeros = self.leading_zeros();
        self.offset += leading_zeros;
        self.length -= leading_zeros;
        if has_cached_unset_bit_count(*self.unset_bit_count_cache.get_mut()) {
            *self.unset_bit_count_cache.get_mut() -= leading_zeros as u64;
        }
        leading_zeros
    }
    /// Take all `1` bits at the start of the [`Bitmap`] before a `0` is seen, returning how many
    /// bits were taken
    pub fn take_leading_ones(&mut self) -> usize {
        if self
            .lazy_unset_bits()
            .is_some_and(|unset_bits| unset_bits == 0)
        {
            let leading_ones = self.length;
            self.offset += self.length;
            self.length = 0;
            *self.unset_bit_count_cache.get_mut() = 0;
            return leading_ones;
        }

        let leading_ones = self.leading_ones();
        self.offset += leading_ones;
        self.length -= leading_ones;
        // @NOTE: the unset_bit_count_cache remains unchanged
        leading_ones
    }
    /// Take all `0` bits at the back of the [`Bitmap`] before a `1` is seen, returning how many
    /// bits were taken
    pub fn take_trailing_zeros(&mut self) -> usize {
        if self
            .lazy_unset_bits()
            .is_some_and(|unset_bits| unset_bits == self.length)
        {
            let trailing_zeros = self.length;
            self.length = 0;
            *self.unset_bit_count_cache.get_mut() = 0;
            return trailing_zeros;
        }

        let trailing_zeros = self.trailing_zeros();
        self.length -= trailing_zeros;
        if has_cached_unset_bit_count(*self.unset_bit_count_cache.get_mut()) {
            *self.unset_bit_count_cache.get_mut() -= trailing_zeros as u64;
        }
        trailing_zeros
    }
    /// Take all `1` bits at the back of the [`Bitmap`] before a `0` is seen, returning how many
    /// bits were taken
    pub fn take_trailing_ones(&mut self) -> usize {
        if self
            .lazy_unset_bits()
            .is_some_and(|unset_bits| unset_bits == 0)
        {
            let trailing_ones = self.length;
            self.length = 0;
            *self.unset_bit_count_cache.get_mut() = 0;
            return trailing_ones;
        }

        let trailing_ones = self.trailing_ones();
        self.length -= trailing_ones;
        // @NOTE: the unset_bit_count_cache remains unchanged
        trailing_ones
    }
}

impl<P: AsRef<[bool]>> From<P> for Bitmap {
    fn from(slice: P) -> Self {
        Self::from_trusted_len_iter(slice.as_ref().iter().copied())
    }
}

impl FromIterator<bool> for Bitmap {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = bool>,
    {
        MutableBitmap::from_iter(iter).into()
    }
}

impl FromTrustedLenIterator<bool> for Bitmap {
    fn from_iter_trusted_length<T: IntoIterator<Item = bool>>(iter: T) -> Self
    where
        T::IntoIter: TrustedLen,
    {
        MutableBitmap::from_trusted_len_iter(iter.into_iter()).into()
    }
}

impl Bitmap {
    /// Creates a new [`Bitmap`] from an iterator of booleans.
    ///
    /// # Safety
    /// The iterator must report an accurate length.
    #[inline]
    pub unsafe fn from_trusted_len_iter_unchecked<I: Iterator<Item = bool>>(iterator: I) -> Self {
        MutableBitmap::from_trusted_len_iter_unchecked(iterator).into()
    }

    /// Creates a new [`Bitmap`] from an iterator of booleans.
    #[inline]
    pub fn from_trusted_len_iter<I: TrustedLen<Item = bool>>(iterator: I) -> Self {
        MutableBitmap::from_trusted_len_iter(iterator).into()
    }

    /// Creates a new [`Bitmap`] from a fallible iterator of booleans.
    #[inline]
    pub fn try_from_trusted_len_iter<E, I: TrustedLen<Item = std::result::Result<bool, E>>>(
        iterator: I,
    ) -> std::result::Result<Self, E> {
        Ok(MutableBitmap::try_from_trusted_len_iter(iterator)?.into())
    }

    /// Creates a new [`Bitmap`] from a fallible iterator of booleans.
    ///
    /// # Safety
    /// The iterator must report an accurate length.
    #[inline]
    pub unsafe fn try_from_trusted_len_iter_unchecked<
        E,
        I: Iterator<Item = std::result::Result<bool, E>>,
    >(
        iterator: I,
    ) -> std::result::Result<Self, E> {
        Ok(MutableBitmap::try_from_trusted_len_iter_unchecked(iterator)?.into())
    }
}

impl<'a> IntoIterator for &'a Bitmap {
    type Item = bool;
    type IntoIter = BitmapIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        BitmapIter::<'a>::new(&self.storage, self.offset, self.length)
    }
}

impl IntoIterator for Bitmap {
    type Item = bool;
    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter::new(self)
    }
}

impl Splitable for Bitmap {
    #[inline(always)]
    fn check_bound(&self, offset: usize) -> bool {
        offset <= self.len()
    }

    unsafe fn _split_at_unchecked(&self, offset: usize) -> (Self, Self) {
        if offset == 0 {
            return (Self::new(), self.clone());
        }
        if offset == self.len() {
            return (self.clone(), Self::new());
        }

        let ubcc = self.unset_bit_count_cache.load();

        let lhs_length = offset;
        let rhs_length = self.length - offset;

        let mut lhs_ubcc = UNKNOWN_BIT_COUNT;
        let mut rhs_ubcc = UNKNOWN_BIT_COUNT;

        if has_cached_unset_bit_count(ubcc) {
            if ubcc == 0 {
                lhs_ubcc = 0;
                rhs_ubcc = 0;
            } else if ubcc == self.length as u64 {
                lhs_ubcc = offset as u64;
                rhs_ubcc = (self.length - offset) as u64;
            } else {
                // If we keep all but a small portion of the array it is worth
                // doing an eager re-count since we can reuse the old count via the
                // inclusion-exclusion principle.
                let small_portion = (self.length / 4).max(32);

                if lhs_length <= rhs_length {
                    if rhs_length + small_portion >= self.length {
                        let count = count_zeros(&self.storage, self.offset, lhs_length) as u64;
                        lhs_ubcc = count;
                        rhs_ubcc = ubcc - count;
                    }
                } else if lhs_length + small_portion >= self.length {
                    let count = count_zeros(&self.storage, self.offset + offset, rhs_length) as u64;
                    lhs_ubcc = ubcc - count;
                    rhs_ubcc = count;
                }
            }
        }

        debug_assert!(lhs_ubcc == UNKNOWN_BIT_COUNT || lhs_ubcc <= ubcc);
        debug_assert!(rhs_ubcc == UNKNOWN_BIT_COUNT || rhs_ubcc <= ubcc);

        (
            Self {
                storage: self.storage.clone(),
                offset: self.offset,
                length: lhs_length,
                unset_bit_count_cache: RelaxedCell::from(lhs_ubcc),
            },
            Self {
                storage: self.storage.clone(),
                offset: self.offset + offset,
                length: rhs_length,
                unset_bit_count_cache: RelaxedCell::from(rhs_ubcc),
            },
        )
    }
}
