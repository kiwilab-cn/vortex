use vortex::scalar::{BoolScalar, Scalar};
use vortex_error::VortexResult;

use crate::array::bool::BoolArray;
use crate::compute::scalar_at::ScalarAtFn;
use crate::validity::ArrayValidity;

impl ScalarAtFn for BoolArray<'_> {
    fn scalar_at(&self, index: usize) -> VortexResult<Scalar> {
        Ok(BoolScalar::try_new(
            self.is_valid(index)
                .then(|| self.boolean_buffer().value(index)),
            self.dtype().nullability(),
        )
        .unwrap()
        .into())
    }
}
