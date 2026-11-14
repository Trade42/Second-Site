//! Port of `LiveLAPermitProvider` (`apps/api/app/integrations/la_permits/provider.py`): LADBS
//! permits from the City's Socrata open-data API. The trait, `RawPermit`, `address_prefix`,
//! `soql_escape` and the fixture provider over the committed real snapshot are `ss-domain`'s.
//!
//! Request shape, as Python sends it: `GET {base}/resource/{dataset}.json` with the caller's
//! parameters followed by `$limit=1000`, `$offset=<n>`, `$order=permit_nbr`, page after page
//! until a page has fewer than 1000 rows, for the electrical then the mechanical dataset.
//! `X-App-Token` is sent only when a token is configured. Retry policy: Python's default
//! (3 attempts, 0.5 s backoff, 20 s timeout).

use serde_json::{Map, Value};
use ss_core::{DataOrigin, IntegrationStatus};
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{
    status_entry, ProviderError, ProviderErrorKind, RetryPolicy,
};
use ss_domain::integrations::la_permits::{
    address_prefix, soql_escape, LaPermitProvider, RawPermit, PROVIDER,
};
use ss_domain::py;

use super::http::HttpClient;

pub const PAGE: usize = 1000;

#[derive(Debug)]
pub struct LiveLaPermitProvider {
    base_url: String,
    datasets: [String; 2],
    client: HttpClient,
}

impl LiveLaPermitProvider {
    pub fn new(
        base_url: &str,
        electrical_dataset: &str,
        mechanical_dataset: &str,
        app_token: Option<&str>,
    ) -> Self {
        Self::with_policy(
            base_url,
            electrical_dataset,
            mechanical_dataset,
            app_token,
            RetryPolicy::default(),
        )
    }

    pub fn with_policy(
        base_url: &str,
        electrical_dataset: &str,
        mechanical_dataset: &str,
        app_token: Option<&str>,
        policy: RetryPolicy,
    ) -> Self {
        // `{"X-App-Token": app_token} if app_token else {}`.
        let headers = match app_token.filter(|t| !t.is_empty()) {
            Some(t) => vec![("X-App-Token".to_owned(), t.to_owned())],
            None => Vec::new(),
        };
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            datasets: [electrical_dataset.to_owned(), mechanical_dataset.to_owned()],
            client: HttpClient::new(PROVIDER, policy, headers),
        }
    }

    fn bad(msg: impl Into<String>) -> ProviderError {
        ProviderError::new(PROVIDER, ProviderErrorKind::BadResponse, msg)
    }

    /// `_query`: every page of one dataset.
    fn query(&self, dataset: &str, params: &[(&str, String)]) -> Result<Vec<RawPermit>, ProviderError> {
        let url = format!("{}/resource/{dataset}.json", self.base_url);
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            let limit = PAGE.to_string();
            let off = offset.to_string();
            let mut q: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
            q.push(("$limit", &limit));
            q.push(("$offset", &off));
            q.push(("$order", "permit_nbr"));
            let rows = self.client.with_retries(|| {
                let resp = self.client.get_once(&url, &q)?;
                match resp.json(PROVIDER)? {
                    Value::Array(rows) => Ok(rows),
                    _ => Err(Self::bad("Socrata response is not a list of rows")),
                }
            })?;
            let n = rows.len();
            for r in rows {
                let record: Map<String, Value> = match r {
                    Value::Object(m) => m,
                    _ => return Err(Self::bad("Socrata row is not an object")),
                };
                let nbr = record
                    .get("permit_nbr")
                    .map_or_else(|| "None".to_owned(), py::value_str);
                out.push(RawPermit {
                    dataset: dataset.to_owned(),
                    source_url: format!("{url}?permit_nbr={nbr}"),
                    record,
                    data_origin: DataOrigin::Live,
                });
            }
            if n < PAGE {
                return Ok(out);
            }
            offset += PAGE;
        }
    }

    fn all_datasets(&self, params: &[(&str, String)]) -> Result<Vec<RawPermit>, ProviderError> {
        let mut out = Vec::new();
        for ds in &self.datasets {
            out.extend(self.query(ds, params)?);
        }
        Ok(out)
    }
}

/// The SoQL `$where` of `search_by_address_prefix`.
pub fn address_where(
    house_number: &str,
    direction: Option<&str>,
    street_name: &str,
    zip_code: Option<&str>,
) -> String {
    let prefix = soql_escape(&address_prefix(house_number, direction, street_name));
    let mut w = format!("upper(primary_address) like '{prefix}%'");
    if let Some(z) = zip_code.filter(|z| !z.is_empty()) {
        w.push_str(&format!(" AND zip_code = '{}'", soql_escape(z)));
    }
    w
}

impl LaPermitProvider for LiveLaPermitProvider {
    fn mode(&self) -> &str {
        "live"
    }

    fn search_by_address_prefix(
        &self,
        house_number: &str,
        direction: Option<&str>,
        street_name: &str,
        zip_code: Option<&str>,
    ) -> Result<Vec<RawPermit>, ProviderError> {
        let w = address_where(house_number, direction, street_name, zip_code);
        self.all_datasets(&[("$where", w)])
    }

    fn get_permit(&self, permit_number: &str) -> Result<Vec<RawPermit>, ProviderError> {
        self.all_datasets(&[("permit_nbr", permit_number.to_owned())])
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            PROVIDER,
            self.mode(),
            IntegrationStatus::Live,
            true,
            "Public Socrata API; app token optional. Coverage 2020-present.",
        )
    }
}
