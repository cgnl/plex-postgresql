use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const SLOT_FREE: u8 = 0;
const SLOT_READY: u8 = 1;

#[derive(Debug)]
struct SimConnection {
    id: i32,
    streaming_active: AtomicBool,
    is_pg_active: bool,
    results_consumed: Cell<i32>,
}

impl SimConnection {
    fn new(id: i32) -> Self {
        Self {
            id,
            streaming_active: AtomicBool::new(false),
            is_pg_active: true,
            results_consumed: Cell::new(0),
        }
    }
}

struct SimSlot {
    conn: Rc<SimConnection>,
    state: AtomicU8,
    owner_thread: std::thread::ThreadId,
}

struct SimPool {
    slots: Vec<SimSlot>,
}

thread_local! {
    static TLS_CACHED_SLOT: Cell<Option<usize>> = const { Cell::new(None) };
}

impl SimPool {
    fn new() -> Self {
        Self { slots: Vec::new() }
    }

    fn add_conn(&mut self, conn: Rc<SimConnection>) {
        self.slots.push(SimSlot {
            conn,
            state: AtomicU8::new(SLOT_READY),
            owner_thread: std::thread::current().id(),
        });
    }

    fn get_connection(&self) -> Option<Rc<SimConnection>> {
        let current = std::thread::current().id();
        if let Some(idx) = TLS_CACHED_SLOT.with(|c| c.get()) {
            if idx < self.slots.len() {
                let slot = &self.slots[idx];
                if slot.state.load(Ordering::Acquire) == SLOT_READY
                    && slot.conn.is_pg_active
                    && !slot.conn.streaming_active.load(Ordering::Acquire)
                {
                    return Some(slot.conn.clone());
                }
            }
        }

        for (idx, slot) in self.slots.iter().enumerate() {
            if slot.state.load(Ordering::Acquire) == SLOT_READY
                && slot.owner_thread == current
                && slot.conn.is_pg_active
                && !slot.conn.streaming_active.load(Ordering::Acquire)
            {
                TLS_CACHED_SLOT.with(|c| c.set(Some(idx)));
                return Some(slot.conn.clone());
            }
        }

        for (idx, slot) in self.slots.iter().enumerate() {
            if slot.state.load(Ordering::Acquire) == SLOT_FREE {
                TLS_CACHED_SLOT.with(|c| c.set(Some(idx)));
                return Some(slot.conn.clone());
            }
        }

        None
    }
}

fn sim_pqexec(conn: &Rc<SimConnection>) {
    if conn.streaming_active.load(Ordering::Acquire) {
        conn.results_consumed.set(conn.results_consumed.get() + 1);
    }
}

fn resolve_column_tables(pool: &SimPool, pg_conn: &Rc<SimConnection>) -> i32 {
    let mut use_conn = pg_conn.clone();
    if pg_conn.streaming_active.load(Ordering::Acquire) {
        if let Some(alt) = pool.get_connection() {
            if !alt.streaming_active.load(Ordering::Acquire) && alt.id != pg_conn.id {
                use_conn = alt;
            } else {
                return -1;
            }
        } else {
            return -1;
        }
    }
    sim_pqexec(&use_conn);
    use_conn.id
}

fn preload_decltype_cache(pool: &SimPool, pg_conn: &Rc<SimConnection>) -> i32 {
    let mut use_conn = pg_conn.clone();
    if pg_conn.streaming_active.load(Ordering::Acquire) {
        if let Some(alt) = pool.get_connection() {
            if !alt.streaming_active.load(Ordering::Acquire) && alt.id != pg_conn.id {
                use_conn = alt;
            } else {
                return -1;
            }
        } else {
            return -1;
        }
    }
    sim_pqexec(&use_conn);
    use_conn.id
}

#[test]
fn pool_skips_streaming_connection_in_fast_path() {
    let mut pool = SimPool::new();
    let conn1 = Rc::new(SimConnection::new(1));
    let conn2 = Rc::new(SimConnection::new(2));

    pool.add_conn(conn1.clone());
    pool.add_conn(conn2.clone());

    TLS_CACHED_SLOT.with(|c| c.set(Some(0)));
    conn1.streaming_active.store(true, Ordering::Release);

    let got = pool.get_connection().unwrap();
    assert_eq!(got.id, 2);
}

#[test]
fn resolve_column_tables_uses_alternate_connection_when_streaming() {
    let mut pool = SimPool::new();
    let conn1 = Rc::new(SimConnection::new(10));
    let conn2 = Rc::new(SimConnection::new(20));

    pool.add_conn(conn1.clone());
    pool.add_conn(conn2.clone());

    conn1.streaming_active.store(true, Ordering::Release);
    let used = resolve_column_tables(&pool, &conn1);
    assert_eq!(used, 20);
}

#[test]
fn preload_decltype_cache_uses_alternate_connection_when_streaming() {
    let mut pool = SimPool::new();
    let conn1 = Rc::new(SimConnection::new(30));
    let conn2 = Rc::new(SimConnection::new(40));

    pool.add_conn(conn1.clone());
    pool.add_conn(conn2.clone());

    conn1.streaming_active.store(true, Ordering::Release);
    let used = preload_decltype_cache(&pool, &conn1);
    assert_eq!(used, 40);
}

#[test]
fn sim_pqexec_on_streaming_marks_consumed() {
    let conn = Rc::new(SimConnection::new(55));
    conn.streaming_active.store(true, Ordering::Release);
    sim_pqexec(&conn);
    assert_eq!(conn.results_consumed.get(), 1);
}
