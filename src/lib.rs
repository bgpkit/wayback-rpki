#[macro_use]
extern crate diesel;

pub mod db;

pub use crate::db::*;

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use chrono::NaiveDate;
use regex::Regex;
use tracing::info;
use rayon::prelude::*;
use crate::db::{roa_history, RoaFile};

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct RoaEntry {
    nic: String,
    prefix: String,
    asn: u32,
    date: NaiveDate,
}


pub fn crawl_nic(nic_url: &str, crawl_all: bool) -> Vec<RoaFile> {
    let fields: Vec<&str> = nic_url.split("/").collect();
    let nic = fields[4].split(".").collect::<Vec<&str>>()[0].to_owned();

    let year_pattern: Regex = Regex::new(r#"<a href=".*"> (....)/</a>.*"#).unwrap();
    let month_day_pattern: Regex = Regex::new(r#"<a href=".*"> (..)/</a>.*"#).unwrap();

    // get all years
    let body = reqwest::blocking::get(nic_url).unwrap().text().unwrap();
    let years: Vec<String> = year_pattern.captures_iter(body.as_str()).map(|cap|{
        cap[1].to_owned()
    }).collect();

    let roa_files = if crawl_all {
        years.par_iter().map(|year| {
            info!("scraping data for {}/{} ...", &nic_url, &year);
            let year_url = format!("{}/{}", nic_url, year);

            let body = reqwest::blocking::get(year_url.as_str()).unwrap().text().unwrap();
            let months: Vec<String> = month_day_pattern.captures_iter(body.as_str()).map(|cap|{
                cap[1].to_owned()
            }).collect();

            months.par_iter().map(|month| {
                info!("scraping data for {}/{} ...", &year_url, &month);
                let month_url = format!("{}/{}", year_url, month);
                let body = reqwest::blocking::get(month_url.as_str()).unwrap().text().unwrap();
                let days: Vec<RoaFile> = month_day_pattern.captures_iter(body.as_str()).map(|cap|{
                    let day = cap[1].to_owned();
                    let url = format!("{}/{}/roas.csv", month_url, day);
                    let file_date = chrono::NaiveDate::from_ymd(year.parse::<i32>().unwrap(), month.parse::<u32>().unwrap(), day.parse::<u32>().unwrap());
                    RoaFile{
                        nic: nic.to_owned(),
                        url,
                        file_date,
                        processed: false
                    }
                }).collect();
                days
            }).flat_map(|x|x).collect::<Vec<RoaFile>>()
        }).flat_map(|x|x).collect::<Vec<RoaFile>>()
    } else {
        let year = years.last().unwrap();
        let year_url = format!("{}/{}", nic_url, year);
        let body = reqwest::blocking::get(year_url.as_str()).unwrap().text().unwrap();
        let months: Vec<String> = month_day_pattern.captures_iter(body.as_str()).map(|cap|{
            cap[1].to_owned()
        }).collect();
        let month = months.last().unwrap();
        let month_url = format!("{}/{}", year_url, month);
        let body = reqwest::blocking::get(month_url.as_str()).unwrap().text().unwrap();

        month_day_pattern.captures_iter(body.as_str()).map(|cap|{
            let day = cap[1].to_owned();
            let url = format!("{}/{}/roas.csv", month_url, day);
            let file_date = chrono::NaiveDate::from_ymd(year.parse::<i32>().unwrap(), month.parse::<u32>().unwrap(), day.parse::<u32>().unwrap());
            RoaFile{
                nic: nic.to_owned(),
                url,
                file_date,
                processed: false
            }
        }).collect::<Vec<RoaFile>>()
    };

    roa_files
}

pub fn parse_roas_csv(csv_url: &str) -> HashSet<RoaEntry> {
    // parse csv url for auxiliary fields
    let fields: Vec<&str> = csv_url.split("/").collect();
    let nic = fields[4].split(".").collect::<Vec<&str>>()[0].to_owned();
    let year = fields[5].parse::<i32>().unwrap();
    let month = fields[6].parse::<u32>().unwrap();
    let day = fields[7].parse::<u32>().unwrap();
    let date = NaiveDate::from_ymd(year, month, day);

    let response = reqwest::blocking::get(csv_url).unwrap();
    let reader = BufReader::new(response);
    let mut roas = HashSet::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break
        };
        if line.starts_with("URI") {
            // skip the first line
            continue
        }
        if line.contains("HTML"){
            info!("file {} does not exist, skipping", csv_url);
            break
        }

        let fields = line.split(",").collect::<Vec<&str>>();
        let asn = fields[1].strip_prefix("AS").unwrap().parse::<u32>().unwrap();
        let prefix = fields[2].to_owned();
        roas.insert(RoaEntry {prefix, asn, nic: nic.to_owned(), date});
    }

    roas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crawl() {
        tracing_subscriber::fmt() .with_max_level(tracing::Level::INFO) .init();
        let roa_files = crawl_nic("https://ftp.ripe.net/rpki/ripencc.tal", false);
        for x in roa_files {
            dbg!(x);
        }
    }

    #[test]
    fn test_parse() {
        tracing_subscriber::fmt() .with_max_level(tracing::Level::INFO) .init();
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/ripencc.tal/2022/01/15/roas.csv");
        for roa in &roas.iter().take(10).collect::<Vec<&RoaEntry>>() {
            println!("{} {}", roa.asn, roa.prefix);
        }
    }
}
