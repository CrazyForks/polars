use crate::chunked_array::object::extension::drop::drop_list;
use crate::prelude::*;

#[inline(never)]
#[cold]
fn drop_slow<T: PolarsDataType>(ca: &mut ChunkedArray<T>) {
    // SAFETY:
    // guarded by the type system
    // the transmute only convinces the type system that we are a list
    #[allow(clippy::transmute_undefined_repr)]
    unsafe {
        drop_list(std::mem::transmute::<&mut ChunkedArray<T>, &ListChunked>(
            ca,
        ))
    }
}

impl<T: PolarsDataType> Drop for ChunkedArray<T> {
    fn drop(&mut self) {
        if matches!(self.dtype(), DataType::List(_)) {
            drop_slow(self);
        }
    }
}
