# Wayback RPKI Database

This project implements the crawler of RIPE RIS RPKI daily dump (https://ftp.ripe.net/rpki/) with a database
schema design to hold historical information.

## RESTful API Access

**ALPHA** version of the API is available at https://alpha.api.bgpkit.com/roas. See the documentation site at https://alpha.api.bgpkit.com/docs/#/bgp/search_roas for more information.

This API is experimental and may change in the future. Please use it with caution. We try to keep the API stable as much as possible. Keep track of the API status at https://status.bgpkit.com/.

If you need batch access to the data or help on setting up a local instance, please contact us at data@bgpkit.com. We will be happy to work with you. No need to crawl the data yourself.

For example: query `https://alpha.api.bgpkit.com/roas?asn=400644` will return the following JSON object
```json
{
  "page": 0,
  "page_size": 100,
  "count": 1,
  "data": [
    {
      "asn": 400644,
      "max_len": 48,
      "prefix": "2620:aa:a000::/48",
      "tal": "arin",
      "current": true,
      "date_ranges": [
        [
          "2023-02-23",
          "2023-04-15"
        ]
      ]
    }
  ]
}
```

Here is how to interpret the fields:
- `asn`: the ASN of the ROA
- `prefix`: the prefix length of the ROA is `2620:aa:a000::/48`
- `max_len`: the maximum length of the prefix is `/48`
- `tal`: the TAL of the ROA is ARIN
- `date_ranges`: the ROA is valid from `2023-02-23` to `2023-04-15`
- `current`: the ROA is still valid in the most recent dump file

## Implementation

### Components

- `wayback-rpki`: the crawler that fetches the daily dump and updates the database
- `wayback-rpki-database`: the PostgreSQL database that holds the historical information

### Database Schema

The project uses PostgreSQL database as it supports IP prefix operations with `CIDR` data type. 

The database schema is defined as follows:
```sql
create table roa_files (
        url text not null,
        tal text not null,
        file_date date not null,
        rows_count integer not null,
        processed boolean not null default false,
        constraint roa_files_pkey primary key (file_date, tal)
);

create table roa_history (
        tal text not null,
        prefix cidr not null,
        asn bigint not null,
        date_ranges daterange[] not null,
        max_len integer not null,
        constraint roa_history_prefix_asn_max_len_key unique (prefix, asn, max_len)
);
```

- the `roa_files` table holds the information about the daily dump files. The `processed` column indicates whether the file has been processed or not.
- the `roa_history` table holds the historical information about the ROAs. The `date_ranges` column holds the date ranges of the ROAs. The `max_len` column holds the maximum length of the prefix. `date_ranges` reflects the time range of which the ROA is valid for.

The deployment requires setting `DATABASE_URL` environment variable.

## Install and Run

To run the database, you need to have PostgreSQL installed and the tables `roa_files` and `roa_history` created.

Then you can install the binary `wayback-rpki` by checking this repo and running `cargo install --path .`.

The command has two subcommands: `bootstrap` and `update`. The only difference is that `bootstrap` mode will start from the beginning of the dump files, while `update` mode will start from the lastest processed file.

You can run the bootstrap for each TAL by running the following commands:
```bash
export DATABASE_URL=postgresql://localhost/wayback-rpki
wayback-rpki bootstrap --chunks 20 --tal ripencc
wayback-rpki bootstrap --chunks 20 --tal lacnic
wayback-rpki bootstrap --chunks 20 --tal apnic
wayback-rpki bootstrap --chunks 20 --tal arin
wayback-rpki bootstrap --chunks 20 --tal afrinic
```
The `--chunks 20` option indicates that the crawler will fetch and process 20 files in parallel at a time. You can adjust this number based on your network speed.

After the initial bootstrap, you can then run the update mode to keep the database up-to-date. Put it in a cron job to run it daily would be sufficient.