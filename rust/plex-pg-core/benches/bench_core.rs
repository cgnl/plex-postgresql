use criterion::{black_box, criterion_group, criterion_main, Criterion};
use plex_pg_core::db_interpose_common::rust_simple_str_replace;
use plex_pg_core::translate;
use std::ffi::CString;

fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in data {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn bench_core(c: &mut Criterion) {
    let queries = [
        "SELECT id, title, rating FROM metadata_items WHERE id = :id",
        "SELECT * FROM metadata_items WHERE library_section_id = :lib AND metadata_type = :type ORDER BY added_at DESC LIMIT 50",
        "UPDATE metadata_items SET updated_at = datetime('now') WHERE id = :id",
        "INSERT INTO metadata_items (title, rating, added_at) VALUES (:title, :rating, datetime('now'))",
        "SELECT m.id, m.title, IFNULL(m.rating, 0) FROM metadata_items m WHERE m.title GLOB '*test*'",
    ];

    c.bench_function("core_translation_throughput", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let sql = queries[i % queries.len()];
            i = i.wrapping_add(1);
            let t = translate(black_box(sql)).unwrap();
            black_box(t.sql);
        })
    });

    let sql = "SELECT id, title, rating, added_at, updated_at, metadata_type, library_section_id FROM metadata_items WHERE library_section_id = $1 AND metadata_type = $2 ORDER BY added_at DESC LIMIT 50";
    c.bench_function("core_hash_fnv1a", |b| {
        b.iter(|| {
            let h = fnv1a_hash(sql.as_bytes());
            black_box(h);
        })
    });

    let replace_sql =
        "SELECT IFNULL(a, 0), IFNULL(b, 1), IFNULL(c, 2) FROM table WHERE IFNULL(d, 3) > 0";
    let old = CString::new("IFNULL").unwrap();
    let new_str = CString::new("COALESCE").unwrap();
    c.bench_function("core_string_replace", |b| {
        b.iter(|| {
            let input = CString::new(replace_sql).unwrap();
            let ptr = rust_simple_str_replace(input.as_ptr(), old.as_ptr(), new_str.as_ptr());
            if !ptr.is_null() {
                unsafe { libc::free(ptr as *mut libc::c_void) };
            }
        })
    });

    const CACHE_SIZE: usize = 64;
    let mut cache = [(0u64, false); CACHE_SIZE];
    for (i, entry) in cache.iter_mut().enumerate() {
        let key = fnv1a_hash(&(i as u64).to_le_bytes());
        *entry = (key, true);
    }

    c.bench_function("core_cache_lookup", |b| {
        let mut hits = 0u64;
        b.iter(|| {
            for i in 0..100 {
                let query_id = if i % 5 == 0 { i } else { i % CACHE_SIZE };
                let key = fnv1a_hash(&(query_id as u64).to_le_bytes());
                let idx = (key as usize) % CACHE_SIZE;
                if cache[idx].1 && cache[idx].0 == key {
                    hits += 1;
                }
            }
            black_box(hits);
        })
    });
}

criterion_group!(benches, bench_core);
criterion_main!(benches);
