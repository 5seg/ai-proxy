use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{params, Connection};

#[derive(Debug, Clone, Copy)]
pub struct Price {
    pub input: f64,
    pub cached: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl Price {
    pub const ZERO: Price = Price {
        input: 0.0,
        cached: 0.0,
        cache_write: 0.0,
        output: 0.0,
    };
}

pub struct Prices {
    rows: Vec<(String, String, Price)>,
}

impl Prices {
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT model, effective_from, input_per_mtok, cached_per_mtok,
                    cache_write_per_mtok, output_per_mtok
             FROM price ORDER BY effective_from DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                Price {
                    input: row.get(2)?,
                    cached: row.get(3)?,
                    cache_write: row.get(4)?,
                    output: row.get(5)?,
                },
            ))
        })?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row?);
        }
        Ok(Self { rows: all })
    }

    pub fn at(&self, model: &str, day: &str) -> Option<Price> {
        self.rows
            .iter()
            .find(|(candidate, from, _)| candidate == model && from.as_str() <= day)
            .map(|(_, _, price)| *price)
    }
}

#[derive(Debug, Clone)]
pub struct RawRow {
    pub day: String,
    pub device: String,
    pub client_ip: String,
    pub key_prefix: String,
    pub model: String,
    pub requests: i64,
    pub input: i64,
    pub cached: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
}

impl RawRow {
    pub fn cost(&self, prices: &Prices) -> f64 {
        let price = prices.at(&self.model, &self.day).unwrap_or(Price::ZERO);
        let billable_input = self.input.saturating_sub(self.cached);
        (billable_input as f64 * price.input
            + self.cached as f64 * price.cached
            + self.cache_write as f64 * price.cache_write
            + self.output as f64 * price.output)
            / 1_000_000.0
    }

    pub fn cache_saving(&self, prices: &Prices) -> f64 {
        let price = prices.at(&self.model, &self.day).unwrap_or(Price::ZERO);
        (self.cached as f64 * (price.input - price.cached).max(0.0)) / 1_000_000.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Day,
    Device,
    Ip,
    Model,
    Key,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Day => "date",
            Group::Device => "device",
            Group::Ip => "client ip",
            Group::Model => "model",
            Group::Key => "key",
        }
    }
}

#[derive(Default, Clone)]
pub struct Agg {
    pub label: String,
    pub requests: i64,
    pub input: i64,
    pub cached: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cost: f64,
    pub saved: f64,
    /// Models seen here that have no price row for their day; their cost is missing from `cost`.
    pub unpriced: std::collections::BTreeSet<String>,
}

impl Agg {
    pub fn hit_rate(&self) -> Option<f64> {
        if self.input <= 0 {
            None
        } else {
            Some(self.cached as f64 / self.input as f64)
        }
    }

    fn add_row(&mut self, row: &RawRow, prices: &Prices) {
        self.requests += row.requests;
        self.input += row.input;
        self.cached += row.cached;
        self.cache_write += row.cache_write;
        self.output += row.output;
        self.reasoning += row.reasoning;
        self.cost += row.cost(prices);
        self.saved += row.cache_saving(prices);
        if prices.at(&row.model, &row.day).is_none() {
            self.unpriced.insert(row.model.clone());
        }
    }
}

pub fn load_rows(conn: &Connection, since_ms: i64, until_ms: i64) -> Result<Vec<RawRow>> {
    let mut stmt = conn.prepare(
        "SELECT date(r.started_at / 1000, 'unixepoch') AS day,
                d.name,
                r.client_ip,
                k.prefix,
                r.model,
                COUNT(*),
                COALESCE(SUM(r.input_tokens), 0),
                COALESCE(SUM(r.cached_input_tokens), 0),
                COALESCE(SUM(r.cache_write_tokens), 0),
                COALESCE(SUM(r.output_tokens), 0),
                COALESCE(SUM(r.reasoning_tokens), 0)
         FROM request r
         JOIN device d ON d.id = r.device_id
         JOIN api_key k ON k.id = r.api_key_id
         WHERE r.started_at >= ?1 AND r.started_at < ?2
         GROUP BY day, r.device_id, r.client_ip, r.api_key_id, r.model",
    )?;
    let rows = stmt.query_map(params![since_ms, until_ms], |row| {
        Ok(RawRow {
            day: row.get(0)?,
            device: row.get(1)?,
            client_ip: row.get(2)?,
            key_prefix: row.get(3)?,
            model: row.get(4)?,
            requests: row.get(5)?,
            input: row.get(6)?,
            cached: row.get(7)?,
            cache_write: row.get(8)?,
            output: row.get(9)?,
            reasoning: row.get(10)?,
        })
    })?;
    let mut all = Vec::new();
    for row in rows {
        all.push(row?);
    }
    Ok(all)
}

pub fn aggregate(rows: &[RawRow], group: Group, prices: &Prices, reverse: bool) -> Vec<Agg> {
    let mut buckets: HashMap<String, Agg> = HashMap::new();
    for row in rows {
        let key = match group {
            Group::Day => row.day.clone(),
            Group::Device => row.device.clone(),
            Group::Ip => row.client_ip.clone(),
            Group::Model => row.model.clone(),
            Group::Key => row.key_prefix.clone(),
        };
        buckets
            .entry(key.clone())
            .or_insert_with(|| Agg {
                label: key,
                ..Agg::default()
            })
            .add_row(row, prices);
    }
    let mut values: Vec<Agg> = buckets.into_values().collect();
    values.sort_by(|left, right| match group {
        Group::Day => {
            if reverse {
                right.label.cmp(&left.label)
            } else {
                left.label.cmp(&right.label)
            }
        }
        _ => right
            .cost
            .partial_cmp(&left.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.requests.cmp(&left.requests)),
    });
    values
}

pub fn totals(rows: &[RawRow], prices: &Prices) -> Agg {
    let mut total = Agg {
        label: "total".to_string(),
        ..Agg::default()
    };
    for row in rows {
        total.add_row(row, prices);
    }
    total
}

/// Sums already-grouped rows into their total.
pub fn fold(aggs: &[Agg]) -> Agg {
    let mut total = Agg {
        label: "total".to_string(),
        ..Agg::default()
    };
    for agg in aggs {
        total.requests += agg.requests;
        total.input += agg.input;
        total.cached += agg.cached;
        total.cache_write += agg.cache_write;
        total.output += agg.output;
        total.reasoning += agg.reasoning;
        total.cost += agg.cost;
        total.saved += agg.saved;
        total.unpriced.extend(agg.unpriced.iter().cloned());
    }
    total
}

pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub right_align: Vec<bool>,
}

impl Table {
    pub fn render(&self) -> String {
        let mut widths: Vec<usize> = self.headers.iter().map(|header| header.chars().count()).collect();
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
        let mut out = String::new();
        out.push_str(&render_line(&self.headers, &widths, &self.right_align));
        out.push('\n');
        for row in &self.rows {
            out.push_str(&render_line(row, &widths, &self.right_align));
            out.push('\n');
        }
        out
    }
}

fn render_line(cells: &[String], widths: &[usize], right_align: &[bool]) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            if right_align.get(index).copied().unwrap_or(false) {
                format!("{cell:>width$}", width = widths[index])
            } else {
                format!("{cell:<width$}", width = widths[index])
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

pub fn fmt_int(value: i64) -> String {
    let negative = value < 0;
    let digits = value.abs().to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if negative {
        format!("-{out}")
    } else {
        out
    }
}

pub fn fmt_usd(value: f64) -> String {
    format!("${value:.6}")
}

pub fn fmt_rate(rate: Option<f64>) -> String {
    match rate {
        Some(rate) => format!("{:.1}%", rate * 100.0),
        None => "-".to_string(),
    }
}

pub fn fmt_cost(agg: &Agg) -> String {
    if agg.unpriced.is_empty() {
        fmt_usd(agg.cost)
    } else {
        format!("{}*", fmt_usd(agg.cost))
    }
}

pub fn agg_row(agg: &Agg) -> Vec<String> {
    vec![
        agg.label.clone(),
        fmt_int(agg.requests),
        fmt_int(agg.input),
        fmt_int(agg.cached),
        fmt_rate(agg.hit_rate()),
        fmt_int(agg.output),
        fmt_cost(agg),
    ]
}

/// Models that had no price row for their day, so the cost above is a lower bound.
pub fn unpriced_models(rows: &[RawRow], prices: &Prices) -> Vec<String> {
    let mut models: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for row in rows {
        if prices.at(&row.model, &row.day).is_none() {
            models.insert(row.model.as_str());
        }
    }
    models.into_iter().map(str::to_string).collect()
}
