use std::any::Any;
use std::fmt::{Display, Formatter};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use coordinator::reader::ReaderIterator;
use coordinator::service::CoordinatorRef;
use datafusion::arrow::array::ArrayRef;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::PhysicalSortExpr;
use datafusion::physical_plan::metrics::ExecutionPlanMetricsSet;
use datafusion::physical_plan::{
    DisplayFormatType, ExecutionPlan, Partitioning, RecordBatchStream, SendableRecordBatchStream,
    Statistics,
};
use futures::{FutureExt, Stream};
use models::predicate::domain::{PredicateRef, TimeRange};
use models::predicate::Split;
use models::schema::{TskvTableSchema, TskvTableSchemaRef};
use spi::QueryError;
use trace::debug;
use tskv::query_iterator::{QueryOption, TableScanMetrics};

#[derive(Debug, Clone)]
pub struct TagScanExec {
    // connection
    // db: CustomDataSource,
    table_schema: TskvTableSchemaRef,
    proj_schema: SchemaRef,
    predicate: PredicateRef,
    coord: CoordinatorRef,

    /// Execution metrics
    metrics: ExecutionPlanMetricsSet,
}

impl TagScanExec {
    pub(crate) fn new(
        table_schema: TskvTableSchemaRef,
        proj_schema: SchemaRef,
        predicate: PredicateRef,
        coord: CoordinatorRef,
    ) -> Self {
        let metrics = ExecutionPlanMetricsSet::new();
        Self {
            table_schema,
            proj_schema,
            predicate,
            coord,
            metrics,
        }
    }

    pub fn predicate(&self) -> PredicateRef {
        self.predicate.clone()
    }
}

impl ExecutionPlan for TagScanExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.proj_schema.clone()
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(1)
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        None
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(TagScanExec {
            table_schema: self.table_schema.clone(),
            proj_schema: self.proj_schema.clone(),
            coord: self.coord.clone(),
            metrics: self.metrics.clone(),
            predicate: self.predicate.clone(),
        }))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        debug!(
            "Start TagScanExec::execute for partition {} of context session_id {} and task_id {:?}",
            partition,
            context.session_id(),
            context.task_id()
        );

        let batch_size = context.session_config().batch_size();

        let metrics = TableScanMetrics::new(&self.metrics, partition, Some(context.memory_pool()));

        let tag_scan_stream = TagScanStream::new(
            self.table_schema.clone(),
            self.schema(),
            self.coord.clone(),
            batch_size,
            self.predicate.clone(),
            metrics,
        )
        .map_err(|err| DataFusionError::External(Box::new(err)))?;

        Ok(Box::pin(tag_scan_stream))
    }

    fn fmt_as(&self, t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                let filter = self.predicate();
                let fields: Vec<_> = self
                    .proj_schema
                    .fields()
                    .iter()
                    .map(|x| x.name().to_owned())
                    .collect::<Vec<String>>();
                write!(
                    f,
                    "TagScan: {}, projection=[{}]",
                    PredicateDisplay(&filter),
                    fields.join(","),
                )
            }
        }
    }

    fn statistics(&self) -> Statistics {
        // TODO
        Statistics::default()
    }

    fn metrics(&self) -> Option<datafusion::physical_plan::metrics::MetricsSet> {
        Some(self.metrics.clone_inner())
    }
}

/// A wrapper to customize PredicateRef display
#[derive(Debug)]
struct PredicateDisplay<'a>(&'a PredicateRef);

impl<'a> Display for PredicateDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let filter = self.0;
        write!(
            f,
            "limit={:?}, predicate={:?}",
            filter.limit(),
            filter.filter(),
        )
    }
}

struct TagRecordBatchStream {
    schema: SchemaRef,
    columns: Option<Vec<ArrayRef>>,
}

impl Stream for TagRecordBatchStream {
    type Item = Result<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.columns
            .take()
            .map(|e| {
                let batch = RecordBatch::try_new(self.schema.clone(), e).map_err(Into::into);
                Poll::Ready(Some(batch))
            })
            .unwrap_or_else(|| Poll::Ready(None))
    }
}

#[allow(dead_code)]
pub struct TagScanStream {
    proj_schema: SchemaRef,
    batch_size: usize,
    coord: CoordinatorRef,

    iterator: ReaderIterator,

    metrics: TableScanMetrics,
}

impl TagScanStream {
    pub fn new(
        table_schema: TskvTableSchemaRef,
        proj_schema: SchemaRef,
        coord: CoordinatorRef,
        batch_size: usize,
        predicate: PredicateRef,
        metrics: TableScanMetrics,
    ) -> Result<Self, QueryError> {
        let mut proj_fileds = Vec::with_capacity(proj_schema.fields().len());
        for field_name in proj_schema.fields().iter().map(|f| f.name()) {
            if let Some(v) = table_schema.column(field_name) {
                proj_fileds.push(v.clone());
            } else {
                return Err(QueryError::CommonError {
                    msg: format!(
                        "tag scan stream build fail, because can't found field: {}",
                        field_name
                    ),
                });
            }
        }
        let proj_table_schema = TskvTableSchema::new(
            table_schema.tenant.clone(),
            table_schema.db.clone(),
            table_schema.name.clone(),
            proj_fileds,
        );

        let split = Split::new(0, table_schema, TimeRange::all(), predicate);

        let option = QueryOption::new(
            batch_size,
            split,
            None,
            proj_schema.clone(),
            proj_table_schema,
            metrics.tskv_metrics(),
        );

        let iterator = coord.tag_scan(option)?;

        Ok(Self {
            proj_schema,
            batch_size,
            coord,
            iterator,
            metrics,
        })
    }

    pub fn with_iterator(
        proj_schema: SchemaRef,
        batch_size: usize,
        coord: CoordinatorRef,
        iterator: ReaderIterator,
        metrics: TableScanMetrics,
    ) -> Self {
        Self {
            proj_schema,
            batch_size,
            coord,
            iterator,
            metrics,
        }
    }
}

impl Stream for TagScanStream {
    type Item = std::result::Result<RecordBatch, DataFusionError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let metrics = &this.metrics;
        let timer = metrics.elapsed_compute().timer();

        let result = match Box::pin(this.iterator.next()).poll_unpin(cx) {
            Poll::Ready(Some(Ok(record_batch))) => match metrics.record_memory(&record_batch) {
                Ok(_) => Poll::Ready(Some(Ok(record_batch))),
                Err(e) => Poll::Ready(Some(Err(e))),
            },
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(DataFusionError::External(Box::new(e)))))
            }
            Poll::Ready(None) => {
                metrics.done();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        };

        timer.done();
        metrics.record_poll(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // todo   (self.data.len(), Some(self.data.len()))
        (0, Some(0))
    }
}

impl RecordBatchStream for TagScanStream {
    fn schema(&self) -> SchemaRef {
        self.proj_schema.clone()
    }
}
