use std::{net::SocketAddr, sync::Arc};

use arrow::json::LineDelimitedWriter;
use datafusion::{
    datasource::DefaultTableSource,
    logical_plan::{LogicalPlan, PlanVisitor, TableScan},
};
use hex::encode;
use log::debug;
use serde_json::json;
use sha2::{Digest, Sha256};
use warp::{hyper::StatusCode, Filter, Reply};

use crate::{
    config::HttpFrontend, context::SeafowlContext, data_types::TableVersionId,
    provider::SeafowlTable,
};

const QUERY_HEADER: &str = "X-Seafowl-Query";
const IF_NONE_MATCH: &str = "If-None-Match";
const ETAG: &str = "ETag";

#[derive(Default)]
struct ETagBuilderVisitor {
    table_versions: Vec<TableVersionId>,
}

impl PlanVisitor for ETagBuilderVisitor {
    type Error = ();

    fn pre_visit(&mut self, plan: &LogicalPlan) -> Result<bool, Self::Error> {
        if let LogicalPlan::TableScan(TableScan { source, .. }) = plan {
            // TODO handle external Parquet tables too
            if let Some(default_table_source) =
                source.as_any().downcast_ref::<DefaultTableSource>()
            {
                if let Some(table) = default_table_source
                    .table_provider
                    .as_any()
                    .downcast_ref::<SeafowlTable>()
                {
                    self.table_versions.push(table.table_version_id)
                }
            }
        }
        Ok(true)
    }
}

fn plan_to_etag(plan: &LogicalPlan) -> String {
    let mut visitor = ETagBuilderVisitor::default();
    plan.accept(&mut visitor).unwrap();

    debug!("Extracted table versions: {:?}", visitor.table_versions);

    let mut hasher = Sha256::new();
    hasher.update(json!(visitor.table_versions).to_string());
    encode(hasher.finalize())
}

// GET /q/[query hash]
pub fn cached_read_query(
    context: Arc<dyn SeafowlContext>,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::path!("q" / String)
        .and(warp::header::<String>(QUERY_HEADER))
        .and(warp::header::optional::<String>(IF_NONE_MATCH))
        .then(move |query_hash, query: String, if_none_match| {
            let context = context.clone();

            async move {
                context.reload_schema().await;
                let mut hasher = Sha256::new();
                hasher.update(&query);
                let hash_str = encode(hasher.finalize());

                debug!(
                    "Received query: {}, URL hash {}, actual hash {}",
                    query, query_hash, hash_str
                );

                // Verify the query hash matches the query
                if query_hash != hash_str {
                    return warp::reply::with_status(
                        "HASH_MISMATCH",
                        StatusCode::BAD_REQUEST,
                    )
                    .into_response();
                }

                // Plan the query
                // TODO handle error
                let plan = context.create_logical_plan(&query).await.unwrap();
                debug!("Query plan: {:?}", plan);

                // Write queries should come in as POST requests
                match plan {
                    LogicalPlan::CreateExternalTable(_)
                    | LogicalPlan::CreateMemoryTable(_)
                    | LogicalPlan::CreateView(_)
                    | LogicalPlan::CreateCatalogSchema(_)
                    | LogicalPlan::CreateCatalog(_)
                    | LogicalPlan::DropTable(_)
                    | LogicalPlan::Analyze(_)
                    | LogicalPlan::Extension(_) => {
                        return warp::reply::with_status(
                            "NOT_READ_ONLY_QUERY",
                            StatusCode::METHOD_NOT_ALLOWED,
                        )
                        .into_response()
                    }
                    _ => (),
                };

                // Pre-execution check: if ETags match, we don't need to re-execute the query
                let etag = plan_to_etag(&plan);
                debug!("ETag: {}, if-none-match header: {:?}", etag, if_none_match);

                if let Some(if_none_match) = if_none_match {
                    if etag == if_none_match {
                        return warp::reply::with_status(
                            "NOT_MODIFIED",
                            StatusCode::NOT_MODIFIED,
                        )
                        .into_response();
                    }
                }

                // Guess we'll have to actually run the query
                let physical = context.create_physical_plan(&plan).await.unwrap();
                let batches = context.collect(physical).await.unwrap();

                let mut buf = Vec::new();
                let mut writer = LineDelimitedWriter::new(&mut buf);
                writer.write_batches(&batches).unwrap();
                writer.finish().unwrap();

                warp::reply::with_header(buf, ETAG, etag).into_response()
            }
        })
}

pub fn filters(
    context: Arc<dyn SeafowlContext>,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    cached_read_query(context)
}

pub async fn run_server(context: Arc<dyn SeafowlContext>, config: HttpFrontend) {
    let filters = filters(context);

    let socket_addr: SocketAddr = format!("{}:{}", config.bind_host, config.bind_port)
        .parse()
        .expect("Error parsing the listen address");
    warp::serve(filters).run(socket_addr).await;
}