use std::path::Path;
use std::sync::Arc;

use arrow::array::RecordBatchReader;
use arrow::datatypes::SchemaRef;
use arrow::pyarrow::{IntoPyArrow, ToPyArrow};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyLong, PyString};
use tokio::fs::File;
use vortex::arrow::infer_schema;
use vortex::Array;
use vortex_dtype::field::Field;
use vortex_dtype::DType;
use vortex_error::VortexResult;
use vortex_sampling_compressor::ALL_COMPRESSORS_CONTEXT;
use vortex_serde::io::{ObjectStoreReadAt, VortexReadAt};
use vortex_serde::layouts::{
    LayoutBatchStream, LayoutContext, LayoutDescriptorReader, LayoutDeserializer,
    LayoutReaderBuilder, Projection, RowFilter, VortexRecordBatchReader,
};

use crate::expr::PyExpr;
use crate::{PyArray, TOKIO_RUNTIME};

pub async fn layout_stream_from_reader<T: VortexReadAt + Unpin>(
    reader: T,
    projection: Projection,
    batch_size: Option<usize>,
    row_filter: Option<RowFilter>,
) -> VortexResult<LayoutBatchStream<T>> {
    let mut builder = LayoutReaderBuilder::new(
        reader,
        LayoutDeserializer::new(
            ALL_COMPRESSORS_CONTEXT.clone(),
            LayoutContext::default().into(),
        ),
    )
    .with_projection(projection);

    if let Some(batch_size) = batch_size {
        builder = builder.with_batch_size(batch_size);
    }

    if let Some(row_filter) = row_filter {
        builder = builder.with_row_filter(row_filter);
    }

    builder.build().await
}

pub async fn read_array_from_reader<T: VortexReadAt + Unpin + 'static>(
    reader: T,
    projection: Projection,
    batch_size: Option<usize>,
    row_filter: Option<RowFilter>,
) -> VortexResult<Array> {
    layout_stream_from_reader(reader, projection, batch_size, row_filter)
        .await?
        .read_all()
        .await
}

pub async fn read_dtype_from_reader<T: VortexReadAt + Unpin>(reader: T) -> VortexResult<DType> {
    LayoutDescriptorReader::new(LayoutDeserializer::new(
        ALL_COMPRESSORS_CONTEXT.clone(),
        LayoutContext::default().into(),
    ))
    .read_footer(&reader, reader.size().await)
    .await?
    .dtype()
}

fn projection_from_python(columns: Option<Vec<Bound<PyAny>>>) -> PyResult<Projection> {
    fn field_from_pyany(field: &Bound<PyAny>) -> PyResult<Field> {
        if field.clone().is_instance_of::<PyString>() {
            Ok(Field::Name(
                field.downcast::<PyString>()?.to_str()?.to_string(),
            ))
        } else if field.is_instance_of::<PyLong>() {
            Ok(Field::Index(field.extract()?))
        } else {
            Err(PyTypeError::new_err(format!(
                "projection: expected list of string, int, and None, but found: {}.",
                field,
            )))
        }
    }

    Ok(match columns {
        None => Projection::All,
        Some(columns) => Projection::Flat(
            columns
                .iter()
                .map(field_from_pyany)
                .collect::<PyResult<Vec<Field>>>()?,
        ),
    })
}

fn row_filter_from_python(row_filter: Option<&Bound<PyExpr>>) -> Option<RowFilter> {
    row_filter.map(|x| RowFilter::new(x.borrow().unwrap().clone()))
}

#[pyclass(name = "TokioFileDataset", module = "io")]
pub struct TokioFileDataset {
    path: String,
    schema: SchemaRef,
}

impl TokioFileDataset {
    async fn file(&self) -> VortexResult<File> {
        Ok(File::open(Path::new(&self.path)).await?)
    }

    pub async fn try_new(path: String) -> VortexResult<Self> {
        let file = File::open(Path::new(&path)).await?;
        let schema = Arc::new(infer_schema(&read_dtype_from_reader(&file).await?)?);

        Ok(Self { path, schema })
    }

    async fn async_to_array(
        &self,
        columns: Option<Vec<Bound<'_, PyAny>>>,
        batch_size: Option<usize>,
        row_filter: Option<&Bound<'_, PyExpr>>,
    ) -> PyResult<PyArray> {
        let inner = read_array_from_reader(
            self.file().await?,
            projection_from_python(columns)?,
            batch_size,
            row_filter_from_python(row_filter),
        )
        .await?;
        Ok(PyArray::new(inner))
    }

    async fn async_to_record_batch_reader(
        self_: PyRef<'_, Self>,
        columns: Option<Vec<Bound<'_, PyAny>>>,
        batch_size: Option<usize>,
        row_filter: Option<&Bound<'_, PyExpr>>,
    ) -> PyResult<PyObject> {
        let layout_reader = layout_stream_from_reader(
            self_.file().await?,
            projection_from_python(columns)?,
            batch_size,
            row_filter_from_python(row_filter),
        )
        .await?;

        let record_batch_reader: Box<dyn RecordBatchReader + Send> = Box::new(
            VortexRecordBatchReader::try_new(layout_reader, &*TOKIO_RUNTIME)?,
        );
        record_batch_reader.into_pyarrow(self_.py())
    }
}

#[pymethods]
impl TokioFileDataset {
    fn schema(self_: PyRef<Self>) -> PyResult<PyObject> {
        self_.schema.clone().to_pyarrow(self_.py())
    }

    #[pyo3(signature = (*, columns=None, batch_size=None, row_filter=None))]
    pub fn to_array(
        &self,
        columns: Option<Vec<Bound<'_, PyAny>>>,
        batch_size: Option<usize>,
        row_filter: Option<&Bound<'_, PyExpr>>,
    ) -> PyResult<PyArray> {
        TOKIO_RUNTIME.block_on(self.async_to_array(columns, batch_size, row_filter))
    }

    #[pyo3(signature = (*, columns=None, batch_size=None, row_filter=None))]
    pub fn to_record_batch_reader(
        self_: PyRef<Self>,
        columns: Option<Vec<Bound<'_, PyAny>>>,
        batch_size: Option<usize>,
        row_filter: Option<&Bound<'_, PyExpr>>,
    ) -> PyResult<PyObject> {
        TOKIO_RUNTIME.block_on(Self::async_to_record_batch_reader(
            self_, columns, batch_size, row_filter,
        ))
    }
}

#[pyclass(name = "ObjectStoreUrlDataset", module = "io")]
pub struct ObjectStoreUrlDataset {
    url: String,
    schema: SchemaRef,
}

impl ObjectStoreUrlDataset {
    async fn reader(&self) -> VortexResult<ObjectStoreReadAt> {
        ObjectStoreReadAt::try_new_from_url(&self.url).await
    }

    pub async fn try_new(url: String) -> VortexResult<Self> {
        let reader = ObjectStoreReadAt::try_new_from_url(&url).await?;
        let schema = Arc::new(infer_schema(&read_dtype_from_reader(&reader).await?)?);

        Ok(Self { url, schema })
    }

    async fn async_to_array(
        &self,
        columns: Option<Vec<Bound<'_, PyAny>>>,
        batch_size: Option<usize>,
        row_filter: Option<&Bound<'_, PyExpr>>,
    ) -> PyResult<PyArray> {
        let inner = read_array_from_reader(
            self.reader().await?,
            projection_from_python(columns)?,
            batch_size,
            row_filter_from_python(row_filter),
        )
        .await?;
        Ok(PyArray::new(inner))
    }

    async fn async_to_record_batch_reader(
        self_: PyRef<'_, Self>,
        columns: Option<Vec<Bound<'_, PyAny>>>,
        batch_size: Option<usize>,
        row_filter: Option<&Bound<'_, PyExpr>>,
    ) -> PyResult<PyObject> {
        let layout_reader = layout_stream_from_reader(
            self_.reader().await?,
            projection_from_python(columns)?,
            batch_size,
            row_filter_from_python(row_filter),
        )
        .await?;

        let record_batch_reader: Box<dyn RecordBatchReader + Send> = Box::new(
            VortexRecordBatchReader::try_new(layout_reader, &*TOKIO_RUNTIME)?,
        );
        record_batch_reader.into_pyarrow(self_.py())
    }
}

#[pymethods]
impl ObjectStoreUrlDataset {
    fn schema(self_: PyRef<Self>) -> PyResult<PyObject> {
        self_.schema.clone().to_pyarrow(self_.py())
    }

    #[pyo3(signature = (*, columns=None, batch_size=None, row_filter=None))]
    pub fn to_array(
        &self,
        columns: Option<Vec<Bound<'_, PyAny>>>,
        batch_size: Option<usize>,
        row_filter: Option<&Bound<'_, PyExpr>>,
    ) -> PyResult<PyArray> {
        TOKIO_RUNTIME.block_on(self.async_to_array(columns, batch_size, row_filter))
    }

    #[pyo3(signature = (*, columns=None, batch_size=None, row_filter=None))]
    pub fn to_record_batch_reader(
        self_: PyRef<Self>,
        columns: Option<Vec<Bound<'_, PyAny>>>,
        batch_size: Option<usize>,
        row_filter: Option<&Bound<'_, PyExpr>>,
    ) -> PyResult<PyObject> {
        TOKIO_RUNTIME.block_on(Self::async_to_record_batch_reader(
            self_, columns, batch_size, row_filter,
        ))
    }
}

#[pyfunction]
pub fn dataset_from_url(url: Bound<PyString>) -> PyResult<ObjectStoreUrlDataset> {
    Ok(TOKIO_RUNTIME.block_on(ObjectStoreUrlDataset::try_new(url.extract()?))?)
}

#[pyfunction]
pub fn dataset_from_path(path: Bound<PyString>) -> PyResult<TokioFileDataset> {
    Ok(TOKIO_RUNTIME.block_on(TokioFileDataset::try_new(path.extract()?))?)
}
