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

fn bench_libpq(c: &mut Criterion) {
    let mut client = match connect() {
        Some(c) => c,
        None => {
            c.bench_function("libpq_skip", |b| b.iter(|| black_box(())));
            return;
        }
    };

    let schema = std::env::var("PLEX_PG_SCHEMA").unwrap_or_else(|_| "plex".to_string());
    let _ = client.simple_query(&format!("SET search_path TO {schema}, public"));

    c.bench_function("pq_exec_simple", |b| {
        b.iter(|| {
            let _ = client.simple_query("SELECT 1");
        })
    });

    c.bench_function("pq_exec_params", |b| {
        b.iter(|| {
            let _ = client.query("SELECT 1 WHERE $1 = 1", &[&1i32]);
        })
    });

    let stmt = match client.prepare("SELECT id, title FROM metadata_items WHERE id = $1") {
        Ok(s) => s,
        Err(_) => {
            c.bench_function("pq_exec_prepared_skip", |b| b.iter(|| black_box(())));
            return;
        }
    };

    c.bench_function("pq_exec_prepared", |b| {
        b.iter(|| {
            let _ = client.query(&stmt, &[&1i32]);
        })
    });

    c.bench_function("pq_exec_prepared_vary", |b| {
        let mut i = 0i32;
        b.iter(|| {
            i = i.wrapping_add(1);
            let _ = client.query(&stmt, &[&((i % 1000) + 1)]);
        })
    });
}

criterion_group!(benches, bench_libpq);
criterion_main!(benches);
