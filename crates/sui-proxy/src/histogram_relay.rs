// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use anyhow::{bail, Result};
use axum::{extract::Extension, http::StatusCode, routing::get, Router};
use prometheus::proto::{Metric, MetricFamily};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceBuilder;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tower_http::LatencyUnit;
use tracing::{info, Level};

const METRICS_ROUTE: &str = "/metrics";

// Creates a new http server that has as a sole purpose to expose
// and endpoint that prometheus agent can use to poll for the metrics.
// A RegistryService is returned that can be used to get access in prometheus Registries.
pub fn start_prometheus_server(addr: SocketAddr) -> HistogramRelay {
    let relay = HistogramRelay::new();
    let app = Router::new()
        .route(METRICS_ROUTE, get(metrics))
        .layer(Extension(relay.clone()))
        .layer(
            ServiceBuilder::new().layer(
                TraceLayer::new_for_http().on_response(
                    DefaultOnResponse::new()
                        .level(Level::INFO)
                        .latency_unit(LatencyUnit::Seconds),
                ),
            ),
        );

    tokio::spawn(async move {
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });
    relay
}

async fn metrics(Extension(relay): Extension<HistogramRelay>) -> (StatusCode, String) {
    let Ok(expformat) = relay.export() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "unable to pop metrics from HistogramRelay".into(),
        );
    };
    (StatusCode::OK, expformat)
}

struct Wrapper(i64, Vec<MetricFamily>);

#[derive(Clone)]
pub struct HistogramRelay(Arc<Mutex<VecDeque<Wrapper>>>);

impl Default for HistogramRelay {
    fn default() -> Self {
        HistogramRelay(Arc::new(Mutex::new(VecDeque::new())))
    }
}
impl HistogramRelay {
    pub fn new() -> Self {
        Self::default()
    }
    /// submit will take metric family submissions and store them for scraping
    /// in doing so, it will also wrap each entry in a timestamp which will be use
    /// for pruning old entires on each submission call. this may not be ideal long term.
    pub fn submit(&self, data: Vec<MetricFamily>) {
        //  represents a collection timestamp
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let mut queue = self
            .0
            .lock()
            .expect("couldn't get mut lock on HistogramRelay");
        queue.retain(|v| (timestamp_ms - v.0) < 300000); // drain anything 5 mins or older
        queue.push_back(Wrapper(timestamp_ms, data));
    }
    pub fn export(&self) -> Result<String> {
        // totally drain all metrics whenever we get a scrape request from the metrics handler
        let mut queue = self
            .0
            .lock()
            .expect("couldn't get mut lock on HistogramRelay");

        let data: Vec<Wrapper> = queue.drain(..).collect();
        info!(
            "histogram queue drained {} items; remaining count {}",
            data.len(),
            queue.len()
        );

        let mut histograms = vec![];
        for mf in data {
            histograms.extend(mf.1);
        }

        let histograms: Vec<MetricFamily> = extract_histograms(histograms).collect();
        let encoder = prometheus::TextEncoder::new();
        let string = match encoder.encode_to_string(&histograms) {
            Ok(s) => s,
            Err(error) => bail!("{error}"),
        };
        Ok(string)
    }
}

fn extract_histograms(data: Vec<MetricFamily>) -> impl Iterator<Item = MetricFamily> {
    data.into_iter().filter_map(|mf| {
        let metrics = mf.get_metric().iter().filter_map(|m| {
            if !m.has_histogram() {
                return None;
            }
            let mut v = Metric::default();
            v.set_label(protobuf::RepeatedField::from_slice(m.get_label()));
            v.set_histogram(m.get_histogram().to_owned());
            v.set_timestamp_ms(m.get_timestamp_ms());
            Some(v)
        });

        let only_histograms = protobuf::RepeatedField::from_iter(metrics);
        if only_histograms.len() == 0 {
            return None;
        }

        let mut v = MetricFamily::default();
        v.set_name(mf.get_name().to_owned());
        v.set_help(mf.get_help().to_owned());
        v.set_field_type(mf.get_field_type());
        v.set_metric(only_histograms);
        Some(v)
    })
}

#[cfg(test)]
mod tests {
    use prometheus::proto;
    use protobuf;

    use crate::{
        histogram_relay::extract_histograms,
        prom_to_mimir::tests::{
            create_counter, create_histogram, create_labels, create_metric_counter,
            create_metric_family, create_metric_histogram,
        },
    };

    #[test]
    fn filter_histograms() {
        struct Test {
            data: Vec<proto::MetricFamily>,
            expected: Vec<proto::MetricFamily>,
        }

        let tests = vec![
            Test {
                data: vec![create_metric_family(
                    "test_counter",
                    "i'm a help message",
                    Some(proto::MetricType::GAUGE),
                    protobuf::RepeatedField::from(vec![create_metric_counter(
                        protobuf::RepeatedField::from_vec(create_labels(vec![
                            ("host", "local-test-validator"),
                            ("network", "unittest-network"),
                        ])),
                        create_counter(2046.0),
                    )]),
                )],
                expected: vec![],
            },
            Test {
                data: vec![create_metric_family(
                    "test_histogram",
                    "i'm a help message",
                    Some(proto::MetricType::HISTOGRAM),
                    protobuf::RepeatedField::from(vec![create_metric_histogram(
                        protobuf::RepeatedField::from_vec(create_labels(vec![
                            ("host", "local-test-validator"),
                            ("network", "unittest-network"),
                        ])),
                        create_histogram(),
                    )]),
                )],
                expected: vec![create_metric_family(
                    "test_histogram",
                    "i'm a help message",
                    Some(proto::MetricType::HISTOGRAM),
                    protobuf::RepeatedField::from(vec![create_metric_histogram(
                        protobuf::RepeatedField::from_vec(create_labels(vec![
                            ("host", "local-test-validator"),
                            ("network", "unittest-network"),
                        ])),
                        create_histogram(),
                    )]),
                )],
            },
        ];

        for test in tests {
            let extracted: Vec<proto::MetricFamily> = extract_histograms(test.data).collect();
            assert_eq!(extracted, test.expected);
        }
    }
}
