// pub mod roas_table;
mod roas_trie;

// pub use crate::roas_table::*;

use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use ipnet::IpNet;
use rayon::prelude::*;
use regex::Regex;
use std::collections::HashSet;
use std::str::FromStr;
use tracing::{debug, info};

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

fn __crawl_years(tal_url: &str) -> Vec<String> {
    let year_pattern: Regex = Regex::new(r#"<a href=".*">\s*(\d\d\d\d)/</a>.*"#).unwrap();

    // get all years
    let body = oneio::read_to_string(tal_url).unwrap();
    let years: Vec<String> = year_pattern
        .captures_iter(body.as_str())
        .map(|cap| cap[1].to_owned())
        .collect();

    years
}

fn __crawl_months_days(months_days_url: &str) -> Vec<String> {
    let month_day_pattern: Regex = Regex::new(r#"<a href=".*">\s*(\d\d)/</a>.*"#).unwrap();

    let body = oneio::read_to_string(months_days_url).unwrap();
    let months_days: Vec<String> = month_day_pattern
        .captures_iter(body.as_str())
        .map(|cap| cap[1].to_owned())
        .collect();

    months_days
}

fn check_date(
    date: NaiveDate,
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
    check_month: bool,
    check_day: bool,
) -> bool {
    let from_match = match from {
        Some(from_date) => {
            date.year() >= from_date.year()
                && (check_month && date.month() >= from_date.month() || !check_month)
                && (check_day && date.day() >= from_date.day() || !check_day)
        }
        None => true,
    };
    let until_match = match until {
        Some(until_date) => {
            date.year() <= until_date.year()
                && (check_month && date.month() <= until_date.month() || !check_month)
                && (check_day && date.day() <= until_date.day() || !check_day)
        }
        None => true,
    };

    from_match && until_match
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
    let fields: Vec<&str> = tal_url.split('/').collect();
    let tal = fields[4].split('.').collect::<Vec<&str>>()[0].to_owned();

    // get all years
    let years: Vec<i32> = __crawl_years(tal_url)
        .into_iter()
        .map(|y| y.parse::<i32>().unwrap())
        .filter(|y| {
            let date = NaiveDate::from_ymd_opt(*y, 1, 1).unwrap();
            check_date(date, from, until, false, false)
        })
        .collect();

    years
        .par_iter()
        .map(|year| {
            info!("scanning roas.csv.xz files for {}/{} ...", &tal_url, &year);
            let year_url = format!("{}/{}", tal_url, year);

            let months: Vec<u32> = __crawl_months_days(year_url.as_str())
                .into_iter()
                .map(|m| m.parse::<u32>().unwrap())
                .filter(|m| {
                    let date = NaiveDate::from_ymd_opt(*year, *m, 1).unwrap();
                    check_date(date, from, until, true, false)
                })
                .collect();

            months
                .par_iter()
                .map(|month| {
                    debug!("scraping data for {}/{:02} ...", &year_url, &month);
                    let month_url = format!("{}/{:02}", year_url, month);

                    let days: Vec<u32> = __crawl_months_days(month_url.as_str())
                        .into_iter()
                        .map(|d| d.parse::<u32>().unwrap())
                        .filter(|d| {
                            let date = NaiveDate::from_ymd_opt(*year, *month, *d).unwrap();
                            check_date(date, from, until, true, true)
                        })
                        .collect();

                    days.into_iter()
                        .map(|day| {
                            let url = format!("{}/{:02}/roas.csv.xz", month_url, day);
                            let file_date = NaiveDate::from_ymd_opt(*year, *month, day).unwrap();
                            RoaFile {
                                tal: tal.clone(),
                                url,
                                file_date,
                                rows_count: 0,
                                processed: false,
                            }
                        })
                        .collect::<Vec<RoaFile>>()
                })
                .flat_map(|x| x)
                .collect::<Vec<RoaFile>>()
        })
        .flat_map(|x| x)
        .collect::<Vec<RoaFile>>()
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
    for line in oneio::read_lines(csv_url)? {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.starts_with("URI") {
            // skip the first line
            continue;
        }
        if line.to_lowercase().contains("html") {
            debug!("file {} does not exist, skipping", csv_url);
            break;
        }

        let fields = line.split(',').collect::<Vec<&str>>();
        let asn = fields[1].trim_start_matches("AS").parse::<u32>()?;
        let prefix = IpNet::from_str(fields[2].to_owned().as_str()).unwrap();
        let max_len = match fields[3].to_owned().parse::<i32>() {
            Ok(l) => l,
            Err(_e) => prefix.prefix_len() as i32,
        };

        roas.insert(RoaEntry {
            prefix,
            asn,
            max_len,
            tal: tal.to_owned(),
            date,
        });
    }

    Ok(roas.into_iter().collect::<Vec<RoaEntry>>())
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
        assert_eq!(
            roa_files[0].file_date,
            after_date + chrono::Duration::days(1)
        );
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
}
