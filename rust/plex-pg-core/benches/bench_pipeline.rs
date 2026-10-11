use criterion::{black_box, criterion_group, criterion_main, Criterion};
use postgres::{Client, NoTls};

fn connect() -> Option<Client> {
    let host = std::env::var("PLEX_PG_HOST").unwrap_or_else(|_| "/tmp".to_string());
    let db = std::env::var("PLEX_PG_DATABASE").unwrap_or_else(|_| "plex".to_string());
    let user = std::env::var("PLEX_PG_USER").unwrap_or_else(|_| "plex".to_string());
    let password = std::env::var("PLEX_PG_PASSWORD").ok();

    let mut cfg = postgres::Config::new();
    cfg.host(&host).dbname(&db).user(&user);
    if let Some(pw) = password {
        if !pw.is_empty() {
            cfg.password(pw);
        }
    }

    cfg.connect(NoTls).ok()
}

fn bench_pipeline(c: &mut Criterion) {
    let mut client = match connect() {
        Some(c) => c,
        None => {
            c.bench_function("pipeline_skip", |b| b.iter(|| black_box(())));
            return;
        }
    };

    let schema = std::env::var("PLEX_PG_SCHEMA").unwrap_or_else(|_| "plex".to_string());
    let _ = client.simple_query(&format!("SET search_path TO {schema}, public"));

    const BATCH: usize = 10;
    let mut batch_sql = String::new();
    for i in 0..BATCH {
        if i > 0 {
            batch_sql.push(';');
        }
        batch_sql.push_str("SELECT 1");
    }

    c.bench_function("pipeline_sequential_simple", |b| {
        b.iter(|| {
            for _ in 0..BATCH {
                let _ = client.simple_query("SELECT 1");
            }
        })
    });

    c.bench_function("pipeline_batched_simple", |b| {
        b.iter(|| {
            let _ = client.simple_query(&batch_sql);
        })
    });
}

criterion_group!(benches, bench_pipeline);
criterion_main!(benches);
