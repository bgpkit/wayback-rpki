#![allow(clippy::nonminimal_bool)]

pub mod api;
pub mod legacy;
pub mod pg_aspa;
pub mod pg_ingest;
mod roas_trie;

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, NaiveDate};
use ipnet::IpNet;
use rayon::prelude::*;
use regex::Regex;
use std::collections::HashSet;
use std::str::FromStr;
use tracing::{debug, info, warn};

pub use api::*;
pub use roas_trie::*;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct RoaEntry {
    tal: String,
    prefix: IpNet,
    max_len: i32,
    asn: u32,
    date: NaiveDate,
}

#[derive(Debug)]
pub struct RoaFile {
    pub url: String,
    pub tal: String,
    pub file_date: NaiveDate,
    pub rows_count: i32,
    pub processed: bool,
}

/// Numeric `<a href="...">NN/</a>` entries of a RIPE directory listing:
/// `width` is 4 for years and 2 for months and days. A listing that cannot be
/// fetched or parsed is an error, never an empty listing: the ingest layer must
/// not read an outage as "the archive published nothing".
fn crawl_links(url: &str, width: usize) -> Result<Vec<u32>> {
    let pattern = format!(r#"<a href=".*">\s*(\d{{{width}}})/</a>.*"#);
    let entry_pattern = Regex::new(pattern.as_str())
        .with_context(|| format!("compile listing pattern {pattern}"))?;
    let body = oneio::read_to_string_lossy(url)
        .with_context(|| format!("fetch directory listing {url}"))?;

    entry_pattern
        .captures_iter(body.as_str())
        .map(|capture| {
            capture[1]
                .parse::<u32>()
                .with_context(|| format!("parse listing entry {:?} of {url}", &capture[1]))
        })
        .collect()
}

fn check_date(
    date: NaiveDate,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
    check_month: bool,
    check_day: bool,
) -> bool {
    // Boundary months only apply to the boundary year itself: comparing
    // `month` across ALL years silently drops whole months (e.g. until
    // 2026-09-03 excluded Oct-Dec of every earlier year).
    let from_match = match from {
        Some(from_date) => {
            if !check_month {
                date.year() >= from_date.year()
            } else if !check_day {
                date.year() > from_date.year()
                    || (date.year() == from_date.year() && date.month() >= from_date.month())
            } else {
                date >= from_date
            }
        }
        None => true,
    };
    let until_match = match until {
        Some(until_date) => {
            if !check_month {
                date.year() <= until_date.year()
            } else if !check_day {
                date.year() < until_date.year()
                    || (date.year() == until_date.year() && date.month() <= until_date.month())
            } else {
                date <= until_date
            }
        }
        None => true,
    };

    from_match && until_match
}

/// A listing that cannot be fetched either fails the crawl (`strict`) or drops
/// just that subtree with a warning, which is what the legacy crawl has always
/// done; the ingest uses the strict form.
fn listing_or_empty(url: &str, width: usize, strict: bool) -> Result<Vec<u32>> {
    let listing = crawl_links(url, width).and_then(|entries| {
        // A readable listing with no numeric entry is a broken response (error
        // page, renamed markup), not an archive that published nothing: reading
        // it as empty would let a run record every day as a gap and succeed.
        if entries.is_empty() {
            return Err(anyhow!("{url} returned no entries"));
        }
        Ok(entries)
    });
    match listing {
        Ok(entries) => Ok(entries),
        Err(error) if strict => Err(error),
        Err(error) => {
            warn!("omitting {url} from the crawl: {error:#}");
            Ok(Vec::new())
        }
    }
}

/// Day files of one artifact in the RIPE archive:
/// `{tal_url}/{year}/{month}/{day}/{artifact}`.
fn crawl_artifact_days(
    tal_url: &str,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
    artifact: &str,
    strict: bool,
) -> Result<Vec<ArchiveFile>> {
    let years: Vec<u32> = listing_or_empty(tal_url, 4, strict)?
        .into_iter()
        .filter(|year| {
            NaiveDate::from_ymd_opt(*year as i32, 1, 1)
                .is_some_and(|date| check_date(date, from, until, false, false))
        })
        .collect();

    let per_year = years
        .par_iter()
        .map(|year| -> Result<Vec<ArchiveFile>> {
            let year = *year as i32;
            info!("scanning {artifact} files for {tal_url}/{year} ...");
            let year_url = format!("{tal_url}/{year}");
            let months: Vec<u32> = listing_or_empty(&year_url, 2, strict)?
                .into_iter()
                .filter(|month| {
                    NaiveDate::from_ymd_opt(year, *month, 1)
                        .is_some_and(|date| check_date(date, from, until, true, false))
                })
                .collect();

            let per_month = months
                .par_iter()
                .map(|month| -> Result<Vec<ArchiveFile>> {
                    debug!("scraping data for {year_url}/{month:02} ...");
                    let month_url = format!("{year_url}/{month:02}");
                    let mut files = Vec::new();
                    for day in listing_or_empty(&month_url, 2, strict)? {
                        let Some(file_date) = NaiveDate::from_ymd_opt(year, *month, day) else {
                            continue;
                        };
                        if check_date(file_date, from, until, true, true) {
                            files.push(ArchiveFile {
                                url: format!("{month_url}/{day:02}/{artifact}"),
                                file_date,
                            });
                        }
                    }
                    Ok(files)
                })
                .collect::<Result<Vec<_>>>()?;

            Ok(per_month.into_iter().flatten().collect())
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(per_year.into_iter().flatten().collect())
}

/// Crawl and return all RIPE ROA file metadata after a given date
///
/// The ROA files URLs has the following format:
/// https://ftp.ripe.net/ripe/rpki/ripencc.tal/2022/08/28/roas.csv.xz
pub fn crawl_tal_after(
    tal_url: &str,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
) -> Vec<RoaFile> {
    match crawl_tal_files(tal_url, from, until) {
        Ok(files) => files,
        Err(error) => {
            warn!("failed to crawl {tal_url}: {error:#}");
            Vec::new()
        }
    }
}

/// Best-effort form of `try_crawl_tal_after`: a listing below the root that
/// cannot be fetched omits that subtree instead of dropping the whole TAL. This
/// is the behaviour the v1 rebuild path has always had.
fn crawl_tal_files(
    tal_url: &str,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
) -> Result<Vec<RoaFile>> {
    let tal = tal_name_from_url(tal_url);
    Ok(
        crawl_artifact_days(tal_url, from, until, ROA_ARTIFACT, false)?
            .into_iter()
            .map(|file| roa_file(file, &tal))
            .collect(),
    )
}

/// `crawl_tal_after` with the crawl failure kept as an error: an ingest run
/// must not read an unreachable listing as "the archive published nothing".
pub fn try_crawl_tal_after(
    tal_url: &str,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
) -> Result<Vec<RoaFile>> {
    let tal = tal_name_from_url(tal_url);
    Ok(
        crawl_artifact_days(tal_url, from, until, ROA_ARTIFACT, true)?
            .into_iter()
            .map(|file| roa_file(file, &tal))
            .collect(),
    )
}

fn roa_file(file: ArchiveFile, tal: &str) -> RoaFile {
    RoaFile {
        tal: tal.to_owned(),
        url: file.url,
        file_date: file.file_date,
        rows_count: 0,
        processed: false,
    }
}

/// TAL name as it appears in its archive URL: `.../afrinic.tal` -> `afrinic`.
fn tal_name_from_url(tal_url: &str) -> String {
    tal_url
        .split('/')
        .nth(4)
        .map(|segment| segment.split('.').next().unwrap_or(segment))
        .unwrap_or("unknown")
        .to_owned()
}

/// Parse a RIPE ROA CSV file and return a set of ROA entries.
pub fn parse_roas_csv(csv_url: &str) -> Result<Vec<RoaEntry>> {
    // parse csv url for auxiliary fields
    let fields: Vec<&str> = csv_url.split('/').collect();

    let tal = fields[4].split('.').collect::<Vec<&str>>()[0].to_owned();
    let year = fields[5].parse::<i32>()?;
    let month = fields[6].parse::<u32>()?;
    let day = fields[7].parse::<u32>()?;
    let date = NaiveDate::from_ymd_opt(year, month, day).unwrap();

    let mut roas = HashSet::new();

    let mut file_ok = false;

    for line in oneio::read_lines_lossy(csv_url)? {
        let line = line.unwrap();

        if line.starts_with("URI") {
            file_ok = true;
            continue;
        }

        if !file_ok {
            return Err(anyhow!("file format incorrect!"));
        }

        let fields = line.split(',').collect::<Vec<&str>>();
        let asn = fields[1].trim_start_matches("AS").parse::<u32>().unwrap();
        let prefix = IpNet::from_str(fields[2].to_owned().as_str()).unwrap();
        let max_len = match fields[3].to_owned().parse::<i32>() {
            Ok(l) => l,
            Err(_e) => prefix.prefix_len() as i32,
        };

        let entry = RoaEntry {
            prefix,
            asn,
            max_len,
            tal: tal.to_owned(),
            date,
        };

        roas.insert(entry);
    }

    Ok(roas.into_iter().collect::<Vec<RoaEntry>>())
}

/// RIPE archive entry point of each RIR TAL.
const TAL_URLS: [(&str, &str); 5] = [
    ("afrinic", "https://ftp.ripe.net/rpki/afrinic.tal"),
    ("lacnic", "https://ftp.ripe.net/rpki/lacnic.tal"),
    ("apnic", "https://ftp.ripe.net/rpki/apnic.tal"),
    ("ripencc", "https://ftp.ripe.net/rpki/ripencc.tal"),
    ("arin", "https://ftp.ripe.net/rpki/arin.tal"),
];

/// Resolve one TAL name to its RIPE archive URL. An unknown name is `None`, so
/// callers can report the bad input instead of tripping the panic inside
/// `get_tal_urls`.
pub fn tal_url(name: &str) -> Option<&'static str> {
    TAL_URLS
        .iter()
        .find(|(tal, _)| *tal == name)
        .map(|(_, url)| *url)
}

/// Names accepted by `tal_url` and `get_tal_urls`.
pub fn tal_names() -> Vec<&'static str> {
    TAL_URLS.iter().map(|(tal, _)| *tal).collect()
}

pub fn get_tal_urls(tal: Option<String>) -> Vec<String> {
    match tal {
        None => TAL_URLS.iter().map(|(_, url)| url.to_string()).collect(),
        Some(tal) => vec![tal_url(tal.as_str())
            .expect(
                r#"can only be one of the following "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic""#,
            )
            .to_string()],
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL ingest support (the `wayback-pg` binary). These additions do not
// change any v1 code path: the trie archive, the HTTP API, and the CLI above
// stay as they are.
// ---------------------------------------------------------------------------

/// ROA payload artifact inside a day directory.
pub const ROA_ARTIFACT: &str = "roas.csv.xz";

/// A daily RIPE RPKI archive file: `roas.csv.xz` for ROAs, `output.json.xz` for
/// ASPA (and router keys).
#[derive(Debug, Clone)]
pub struct ArchiveFile {
    pub url: String,
    pub file_date: NaiveDate,
}

/// Crawl RIPE's directory listings and return daily archive files for one
/// artifact in a range. `crawl_tal_after` is the `roas.csv.xz` special case;
/// ASPA needs the same listing walk for `output.json.xz`. A crawl failure is
/// logged and reported as an empty vector; `try_crawl_tal_artifact` keeps it.
pub fn crawl_tal_artifact(
    tal_url: &str,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
    artifact: &str,
) -> Vec<ArchiveFile> {
    match crawl_artifact_days(tal_url, from, until, artifact, false) {
        Ok(files) => files,
        Err(error) => {
            warn!("failed to crawl {tal_url} for {artifact}: {error:#}");
            Vec::new()
        }
    }
}

/// `crawl_tal_artifact` with the crawl failure kept as an error: a run that
/// cannot read the listing has to fail instead of ingesting nothing.
pub fn try_crawl_tal_artifact(
    tal_url: &str,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
    artifact: &str,
) -> Result<Vec<ArchiveFile>> {
    crawl_artifact_days(tal_url, from, until, artifact, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        let roas =
            parse_roas_csv("https://ftp.ripe.net/rpki/ripencc.tal/2022/01/15/roas.csv.xz").unwrap();
        for roa in roas.iter().take(10) {
            println!("{} {} {}", roa.asn, roa.prefix, roa.max_len);
        }
    }

    #[test]
    fn test_crawl_after() {
        let after_date = NaiveDate::from_ymd_opt(2023, 3, 31).unwrap();
        let roa_files = crawl_tal_after(
            "https://ftp.ripe.net/rpki/ripencc.tal",
            Some(after_date),
            None,
        );
        assert!(!roa_files.is_empty());
        assert_eq!(roa_files[0].file_date, after_date);
    }

    #[test]
    fn test_crawl_after_bootstrap() {
        let roa_files = crawl_tal_after("https://ftp.ripe.net/rpki/ripencc.tal", None, None);
        assert!(!roa_files.is_empty());
        assert_eq!(
            roa_files[0].file_date,
            NaiveDate::from_ymd_opt(2011, 1, 21).unwrap()
        );
    }

    #[test]
    fn a_listing_without_entries_is_not_an_empty_archive() {
        let path =
            std::env::temp_dir().join(format!("wayback-rpki-listing-{}.html", std::process::id()));
        std::fs::write(&path, "<html><body>404 Not Found</body></html>").expect("write fixture");
        let url = path.to_string_lossy().into_owned();

        assert!(listing_or_empty(&url, 4, true).is_err());
        assert_eq!(
            listing_or_empty(&url, 4, false).expect("best effort"),
            Vec::<u32>::new()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_broken_listing_drops_its_subtree_only_when_not_strict() {
        let missing = "/nonexistent/wayback-rpki-listing";
        assert!(listing_or_empty(missing, 4, true).is_err());
        assert_eq!(
            listing_or_empty(missing, 4, false).expect("best-effort listing"),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn legacy_crawl_stays_best_effort_while_the_strict_form_fails() {
        let missing = "/nonexistent/wayback-rpki-crawl-test";
        // The ingest must see the failure ...
        assert!(try_crawl_tal_after(missing, None, None).is_err());
        assert!(try_crawl_tal_artifact(missing, None, None, ROA_ARTIFACT).is_err());
        // ... while the v1 wrappers keep their old contract: warn and continue.
        assert!(crawl_tal_after(missing, None, None).is_empty());
        assert!(crawl_tal_artifact(missing, None, None, ROA_ARTIFACT).is_empty());
    }

    #[test]
    fn test_missing_prefix() {
        let roas =
            parse_roas_csv("https://ftp.ripe.net/rpki/ripencc.tal/2024/06/02/roas.csv.xz").unwrap();
        for entry in roas {
            if entry.prefix.to_string().as_str() == "193.0.14.0/24" {
                dbg!(entry);
            }
        }
    }
}

#[cfg(test)]
mod check_date_tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn until_boundary_month_excludes_only_boundary_year() {
        // Regression: until 2026-09-03 must NOT drop Oct-Dec of earlier years.
        let until = d("2026-09-03");
        assert!(check_date(d("2025-10-24"), None, Some(until), true, true));
        assert!(check_date(d("2020-12-31"), None, Some(until), true, true));
        assert!(!check_date(d("2026-10-01"), None, Some(until), true, true));
        // month-grain (used for month listing)
        assert!(check_date(d("2025-12-01"), None, Some(until), true, false));
        assert!(!check_date(d("2026-10-01"), None, Some(until), true, false));
    }

    #[test]
    fn from_boundary_month_excludes_only_boundary_year() {
        let from = d("2015-03-10");
        assert!(check_date(d("2016-01-05"), Some(from), None, true, true));
        assert!(check_date(d("2016-01-01"), Some(from), None, true, false));
        assert!(!check_date(d("2015-02-28"), Some(from), None, true, true));
    }
}
