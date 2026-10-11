#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimStatus {
    SingleTuple,
    TuplesOk,
    FatalError,
    CommandOk,
}

#[derive(Clone, Debug)]
struct SimResult {
    status: SimStatus,
    num_rows: i32,
    num_cols: i32,
    freed: bool,
}

struct SimConn {
    results: Vec<SimResult>,
    next_result: usize,
    single_row_mode: bool,
    in_use_by_streaming: bool,
}

impl SimConn {
    fn new() -> Self {
        Self {
            results: Vec::new(),
            next_result: 0,
            single_row_mode: false,
            in_use_by_streaming: false,
        }
    }

    fn queue_single_rows(&mut self, rows: i32, cols: i32) {
        self.results.clear();
        self.next_result = 0;
        self.single_row_mode = true;
        for _ in 0..rows {
            self.results.push(SimResult {
                status: SimStatus::SingleTuple,
                num_rows: 1,
                num_cols: cols,
                freed: false,
            });
        }
        self.results.push(SimResult {
            status: SimStatus::TuplesOk,
            num_rows: 0,
            num_cols: cols,
            freed: false,
        });
    }

    fn queue_zero_rows(&mut self, cols: i32) {
        self.results.clear();
        self.next_result = 0;
        self.single_row_mode = true;
        self.results.push(SimResult {
            status: SimStatus::TuplesOk,
            num_rows: 0,
            num_cols: cols,
            freed: false,
        });
    }

    fn queue_error_after_rows(&mut self, rows: i32, cols: i32) {
        self.results.clear();
        self.next_result = 0;
        self.single_row_mode = true;
        for _ in 0..rows {
            self.results.push(SimResult {
                status: SimStatus::SingleTuple,
                num_rows: 1,
                num_cols: cols,
                freed: false,
            });
        }
        self.results.push(SimResult {
            status: SimStatus::FatalError,
            num_rows: 0,
            num_cols: 0,
            freed: false,
        });
    }

    fn get_result(&mut self) -> Option<usize> {
        if self.next_result >= self.results.len() {
            return None;
        }
        let idx = self.next_result;
        self.next_result += 1;
        Some(idx)
    }

    fn clear(&mut self, idx: usize) {
        if let Some(r) = self.results.get_mut(idx) {
            r.freed = true;
        }
    }

    fn set_single_row_mode(&mut self) -> bool {
        if self.single_row_mode {
            self.single_row_mode = false;
            return true;
        }
        false
    }
}

struct SimStmt {
    streaming_mode: bool,
    current_result: Option<usize>,
    current_row: i32,
    num_rows: i32,
    num_cols: i32,
    read_done: bool,
}

impl SimStmt {
    fn new() -> Self {
        Self {
            streaming_mode: false,
            current_result: None,
            current_row: 0,
            num_rows: 0,
            num_cols: 0,
            read_done: false,
        }
    }
}

const SIM_SQLITE_ROW: i32 = 101;
const SIM_SQLITE_DONE: i32 = 100;

fn sim_step_streaming(stmt: &mut SimStmt, conn: &mut SimConn) -> i32 {
    if let Some(idx) = stmt.current_result.take() {
        conn.clear(idx);
    }

    let idx = match conn.get_result() {
        Some(i) => i,
        None => {
            stmt.streaming_mode = false;
            conn.in_use_by_streaming = false;
            stmt.read_done = true;
            return SIM_SQLITE_DONE;
        }
    };

    match conn.results[idx].status {
        SimStatus::SingleTuple => {
            stmt.current_result = Some(idx);
            stmt.current_row = 0;
            stmt.num_rows = 1;
            stmt.num_cols = conn.results[idx].num_cols;
            SIM_SQLITE_ROW
        }
        SimStatus::TuplesOk => {
            conn.clear(idx);
            while let Some(extra) = conn.get_result() {
                conn.clear(extra);
            }
            stmt.streaming_mode = false;
            conn.in_use_by_streaming = false;
            stmt.read_done = true;
            SIM_SQLITE_DONE
        }
        SimStatus::FatalError => {
            conn.clear(idx);
            while let Some(extra) = conn.get_result() {
                conn.clear(extra);
            }
            stmt.streaming_mode = false;
            conn.in_use_by_streaming = false;
            stmt.read_done = true;
            SIM_SQLITE_DONE
        }
        SimStatus::CommandOk => {
            conn.clear(idx);
            stmt.streaming_mode = false;
            conn.in_use_by_streaming = false;
            stmt.read_done = true;
            SIM_SQLITE_DONE
        }
    }
}

fn sim_step_first(stmt: &mut SimStmt, conn: &mut SimConn) -> i32 {
    if !conn.set_single_row_mode() {
        return -1;
    }

    stmt.streaming_mode = true;
    conn.in_use_by_streaming = true;

    let idx = match conn.get_result() {
        Some(i) => i,
        None => {
            stmt.streaming_mode = false;
            conn.in_use_by_streaming = false;
            stmt.read_done = true;
            return SIM_SQLITE_DONE;
        }
    };

    match conn.results[idx].status {
        SimStatus::SingleTuple => {
            stmt.current_result = Some(idx);
            stmt.current_row = 0;
            stmt.num_rows = 1;
            stmt.num_cols = conn.results[idx].num_cols;
            SIM_SQLITE_ROW
        }
        SimStatus::TuplesOk => {
            conn.clear(idx);
            stmt.streaming_mode = false;
            conn.in_use_by_streaming = false;
            stmt.read_done = true;
            SIM_SQLITE_DONE
        }
        SimStatus::FatalError => {
            conn.clear(idx);
            stmt.streaming_mode = false;
            conn.in_use_by_streaming = false;
            stmt.read_done = true;
            SIM_SQLITE_DONE
        }
        SimStatus::CommandOk => {
            conn.clear(idx);
            stmt.streaming_mode = false;
            conn.in_use_by_streaming = false;
            stmt.read_done = true;
            SIM_SQLITE_DONE
        }
    }
}

#[test]
fn streaming_transitions_single_rows() {
    let mut conn = SimConn::new();
    conn.queue_single_rows(3, 2);
    assert_eq!(
        conn.results
            .iter()
            .map(|result| result.num_rows)
            .collect::<Vec<_>>(),
        [1, 1, 1, 0]
    );
    let mut stmt = SimStmt::new();

    let rc = sim_step_first(&mut stmt, &mut conn);
    assert_eq!(rc, SIM_SQLITE_ROW);
    assert!(stmt.streaming_mode);
    assert!(conn.in_use_by_streaming);

    let mut rows = 1;
    loop {
        let step = sim_step_streaming(&mut stmt, &mut conn);
        if step == SIM_SQLITE_ROW {
            rows += 1;
            continue;
        }
        assert_eq!(step, SIM_SQLITE_DONE);
        break;
    }

    assert_eq!(rows, 3);
    assert!(!stmt.streaming_mode);
    assert!(!conn.in_use_by_streaming);
    assert!(stmt.read_done);
}

#[test]
fn streaming_zero_rows_done_immediately() {
    let mut conn = SimConn::new();
    conn.queue_zero_rows(4);
    let mut stmt = SimStmt::new();

    let rc = sim_step_first(&mut stmt, &mut conn);
    assert_eq!(rc, SIM_SQLITE_DONE);
    assert!(!stmt.streaming_mode);
    assert!(stmt.read_done);
}

#[test]
fn streaming_error_after_rows_drains_and_finishes() {
    let mut conn = SimConn::new();
    conn.queue_error_after_rows(2, 1);
    let mut stmt = SimStmt::new();

    let rc = sim_step_first(&mut stmt, &mut conn);
    assert_eq!(rc, SIM_SQLITE_ROW);

    let rc2 = sim_step_streaming(&mut stmt, &mut conn);
    assert_eq!(rc2, SIM_SQLITE_ROW);

    let rc3 = sim_step_streaming(&mut stmt, &mut conn);
    assert_eq!(rc3, SIM_SQLITE_DONE);
    assert!(!stmt.streaming_mode);
    assert!(stmt.read_done);
}

#[test]
fn streaming_fallback_when_single_row_mode_fails() {
    let mut conn = SimConn::new();
    conn.single_row_mode = false;
    let mut stmt = SimStmt::new();

    let rc = sim_step_first(&mut stmt, &mut conn);
    assert_eq!(rc, -1);
    assert!(!stmt.streaming_mode);
}

#[test]
fn streaming_connection_exclusive_lifecycle() {
    let mut conn = SimConn::new();
    conn.queue_single_rows(1, 1);
    let mut stmt = SimStmt::new();

    assert!(!conn.in_use_by_streaming);
    let rc = sim_step_first(&mut stmt, &mut conn);
    assert_eq!(rc, SIM_SQLITE_ROW);
    assert!(conn.in_use_by_streaming);

    let rc2 = sim_step_streaming(&mut stmt, &mut conn);
    assert_eq!(rc2, SIM_SQLITE_DONE);
    assert!(!conn.in_use_by_streaming);
}

#[test]
fn streaming_command_result_releases_connection_and_result() {
    for first_step in [true, false] {
        let mut conn = SimConn::new();
        conn.single_row_mode = true;
        conn.in_use_by_streaming = !first_step;
        conn.results.push(SimResult {
            status: SimStatus::CommandOk,
            num_rows: 0,
            num_cols: 0,
            freed: false,
        });
        let mut stmt = SimStmt::new();
        stmt.streaming_mode = !first_step;
        let rc = if first_step {
            sim_step_first(&mut stmt, &mut conn)
        } else {
            sim_step_streaming(&mut stmt, &mut conn)
        };
        assert_eq!(rc, SIM_SQLITE_DONE);
        assert!(conn.results[0].freed);
        assert!(!conn.in_use_by_streaming);
        assert!(!stmt.streaming_mode);
        assert!(stmt.read_done);
    }
}
