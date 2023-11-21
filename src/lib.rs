pub mod db;
pub mod roas_table;

pub use crate::db::*;
pub use crate::roas_table::*;

use chrono::{Datelike, NaiveDate};
use ipnetwork::IpNetwork;
use rayon::prelude::*;
use regex::Regex;
use std::collections::HashSet;
use std::str::FromStr;
use tracing::{debug, info};

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct RoaEntry {
    tal: String,
    prefix: IpNetwork,
    max_len: i32,
    asn: u32,
    date: NaiveDate,
}

fn __crawl_years(tal_url: &str) -> Vec<String> {
    let year_pattern: Regex = Regex::new(r#"<a href=".*">\s*(\d\d\d\d)/</a>.*"#).unwrap();

    // get all years
    let body = reqwest::blocking::get(tal_url).unwrap().text().unwrap();
    let years: Vec<String> = year_pattern
        .captures_iter(body.as_str())
        .map(|cap| cap[1].to_owned())
        .collect();

    years
}

fn __crawl_months_days(months_days_url: &str) -> Vec<String> {
    let month_day_pattern: Regex = Regex::new(r#"<a href=".*">\s*(\d\d)/</a>.*"#).unwrap();

    let body = reqwest::blocking::get(months_days_url)
        .unwrap()
        .text()
        .unwrap();
    let months_days: Vec<String> = month_day_pattern
        .captures_iter(body.as_str())
        .map(|cap| cap[1].to_owned())
        .collect();

    months_days
}

/// Crawl and return all RIPE ROA file meta data after a given date
///
/// The ROA files URLs has the following format:
/// https://ftp.ripe.net/ripe/rpki/ripencc.tal/2022/08/28/roas.csv.xz
pub fn crawl_tal_after(tal_url: &str, after: Option<NaiveDate>) -> Vec<RoaFile> {
    let fields: Vec<&str> = tal_url.split('/').collect();
    let tal = fields[4].split('.').collect::<Vec<&str>>()[0].to_owned();

    let min_date: NaiveDate = NaiveDate::from_ymd_opt(1000, 1, 1).unwrap();
    let after_date: NaiveDate = match after {
        None => min_date,
        Some(d) => d,
    };

    // get all years
    let years: Vec<i32> = __crawl_years(tal_url)
        .into_iter()
        .map(|y| y.parse::<i32>().unwrap())
        .filter(|y| {
            let date = NaiveDate::from_ymd_opt(*y, 1, 1).unwrap();
            date >= NaiveDate::from_ymd_opt(after_date.year(), 1, 1).unwrap()
        })
        .collect();

    years
        .par_iter()
        .map(|year| {
            info!("scraping data for {}/{} ...", &tal_url, &year);
            let year_url = format!("{}/{}", tal_url, year);

            let months: Vec<u32> = __crawl_months_days(year_url.as_str())
                .into_iter()
                .map(|m| m.parse::<u32>().unwrap())
                .filter(|m| {
                    let date = NaiveDate::from_ymd_opt(*year, *m, 1).unwrap();
                    date >= NaiveDate::from_ymd_opt(after_date.year(), after_date.month(), 1)
                        .unwrap()
                })
                .collect();

            months
                .par_iter()
                .map(|month| {
                    info!("scraping data for {}/{:02} ...", &year_url, &month);
                    let month_url = format!("{}/{:02}", year_url, month);

                    let days: Vec<u32> = __crawl_months_days(month_url.as_str())
                        .into_iter()
                        .map(|d| d.parse::<u32>().unwrap())
                        .filter(|d| {
                            let date = NaiveDate::from_ymd_opt(*year, *month, *d).unwrap();
                            date > after_date
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
pub fn parse_roas_csv(csv_url: &str) -> HashSet<RoaEntry> {
    // parse csv url for auxiliary fields
    let fields: Vec<&str> = csv_url.split('/').collect();
    let tal = fields[4].split('.').collect::<Vec<&str>>()[0].to_owned();
    let year = fields[5].parse::<i32>().unwrap();
    let month = fields[6].parse::<u32>().unwrap();
    let day = fields[7].parse::<u32>().unwrap();
    let date = NaiveDate::from_ymd_opt(year, month, day).unwrap();

    let mut roas = HashSet::new();
    for line in oneio::read_lines(csv_url).unwrap() {
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
        let asn = fields[1]
            .strip_prefix("AS")
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let prefix = IpNetwork::from_str(fields[2].to_owned().as_str()).unwrap();
        let max_len = match fields[3].to_owned().parse::<i32>() {
            Ok(l) => l,
            Err(_e) => prefix.prefix() as i32,
        };

        roas.insert(RoaEntry {
            prefix,
            asn,
            max_len,
            tal: tal.to_owned(),
            date,
        });
    }

    roas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/ripencc.tal/2022/01/15/roas.csv.xz");
        for roa in &roas.iter().take(10).collect::<Vec<&RoaEntry>>() {
            println!("{} {} {}", roa.asn, roa.prefix, roa.max_len);
        }
    }

    #[test]
    fn test_crawl_after() {
        let after_date = NaiveDate::from_ymd_opt(2023, 3, 31).unwrap();
        let roa_files = crawl_tal_after("https://ftp.ripe.net/rpki/ripencc.tal", Some(after_date));
        assert!(!roa_files.is_empty());
        assert_eq!(
            roa_files[0].file_date,
            after_date + chrono::Duration::days(1)
        );
    }

    #[test]
    fn test_crawl_after_bootstrap() {
        let roa_files = crawl_tal_after("https://ftp.ripe.net/rpki/ripencc.tal", None);
        assert!(!roa_files.is_empty());
        assert_eq!(
            roa_files[0].file_date,
            NaiveDate::from_ymd_opt(2011, 1, 21).unwrap()
        );
    }
}
