//! Runs CPython for WASI from `python/`, fetched by `mise run python` (or `FRONDE_PYTHON_DIR`).

use fronde_hub::tools::ToolOutput;
use fronde_wasm::{ComponentHost, Limits, Python};
use serde_json::{Value, json};
use sha2::Digest;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn python_dir() -> PathBuf {
    let dir = std::env::var_os("FRONDE_PYTHON_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../python")));
    assert!(
        dir.join("python.wasm").exists(),
        "{} has no python.wasm: run `mise run python`",
        dir.display()
    );
    dir
}

async fn python() -> Python {
    let host = ComponentHost::new().unwrap();
    let started = Instant::now();
    let python = Python::load(&host, &python_dir()).await.unwrap();
    eprintln!("load: {:?}", started.elapsed());
    python
}

async fn run(python: &Python, code: &str) -> ToolOutput {
    python.run(code, None, "Europe/Paris").await
}

fn value(out: &ToolOutput) -> &Value {
    assert!(!out.is_error, "{}", out.to_text());
    &out.value
}

#[tokio::test]
async fn adds_numbers() {
    let python = python().await;
    let out = run(&python, "1+1").await;
    assert_eq!(value(&out), &json!({"value": 2}));
    let started = Instant::now();
    let out = run(&python, "print(1+1)").await;
    eprintln!("print(1+1) after load: {:?}", started.elapsed());
    assert_eq!(value(&out), &json!({"stdout": "2\n"}));
}

fn error(out: &ToolOutput) -> String {
    assert!(out.is_error, "expected an error, got {}", out.to_text());
    out.value.as_str().unwrap().to_string()
}

#[tokio::test]
async fn reads_data_and_returns_last_expression() {
    let python = python().await;
    let data = json!([
        {"category": "food", "amount": 3},
        {"category": "food", "amount": 10},
        {"category": "food", "amount": 5},
        {"category": "rent", "amount": 700},
    ]);
    let code = "import statistics\n\
                from collections import defaultdict\n\
                by = defaultdict(list)\n\
                for row in data:\n    by[row['category']].append(row['amount'])\n\
                {k: statistics.median(v) for k, v in by.items()}";
    let out = python.run(code, Some(&data), "UTC").await;
    assert_eq!(value(&out), &json!({"value": {"food": 5, "rent": 700}}));
}

#[tokio::test]
async fn result_variable_when_no_trailing_expression() {
    let python = python().await;
    let out = run(&python, "result = sum(range(5))\nother = 1").await;
    assert_eq!(value(&out), &json!({"value": 10}));
    let out = run(&python, "x = 1").await;
    assert_eq!(value(&out), &json!({}));
}

#[tokio::test]
async fn captures_prints() {
    let python = python().await;
    let out = run(&python, "print('a')\nprint('b', 2)\n3").await;
    assert_eq!(value(&out), &json!({"value": 3, "stdout": "a\nb 2\n"}));
}

#[tokio::test]
async fn returns_plain_json() {
    let python = python().await;
    let code = "from decimal import Decimal\n\
                from fractions import Fraction\n\
                import datetime\n\
                {\n\
                  'decimal': Decimal('0.1') + Decimal('0.2'),\n\
                  'fraction': Fraction(1, 3),\n\
                  'date': datetime.date(2026, 11, 17),\n\
                  'datetime': datetime.datetime(2026, 11, 17, 8, 30),\n\
                  'set': {1},\n\
                  'tuple': (1, 'a'),\n\
                  'nan': float('nan'),\n\
                  'inf': -float('inf'),\n\
                  1: 'int key',\n\
                  'object': object.__new__(type('Thing', (), {'__repr__': lambda s: 'Thing()'})),\n\
                }";
    let out = run(&python, code).await;
    assert_eq!(
        value(&out),
        &json!({"value": {
            "decimal": "0.3",
            "fraction": "1/3",
            "date": "2026-11-17",
            "datetime": "2026-11-17T08:30:00",
            "set": [1],
            "tuple": [1, "a"],
            "nan": "nan",
            "inf": "-inf",
            "1": "int key",
            "object": "Thing()",
        }})
    );
}

#[tokio::test]
async fn knows_the_user_time_zone() {
    let python = python().await;
    let code = "from datetime import datetime\n\
                datetime(2026, 11, 17, 12, tzinfo=TZ).isoformat()";
    let out = run(&python, code).await;
    assert_eq!(value(&out), &json!({"value": "2026-11-17T12:00:00+01:00"}));
    // An unknown zone falls back to UTC rather than failing every run.
    let out = python.run("str(TZ)", None, "Nowhere/Atlantis").await;
    assert_eq!(value(&out), &json!({"value": "UTC"}));
}

#[tokio::test]
async fn python_error_names_the_line() {
    let python = python().await;
    let out = run(&python, "print('before')\nx = 1\ny = 0\nx / y").await;
    let e = error(&out);
    assert!(
        e.starts_with("ZeroDivisionError: division by zero (line 4); fix the code"),
        "{e}"
    );
    assert!(e.ends_with("printed before the error:\nbefore\n"), "{e}");
    assert!(out.next.unwrap().contains("standard library"));
}

#[tokio::test]
async fn syntax_error_names_the_line() {
    let python = python().await;
    let e = error(&run(&python, "x = 1\ny = = 2").await);
    assert!(e.starts_with("SyntaxError: invalid syntax (line 2)"), "{e}");
}

#[tokio::test]
async fn sys_exit_is_not_a_crash() {
    let python = python().await;
    let out = run(&python, "import sys\nresult = 5\nsys.exit()").await;
    assert_eq!(value(&out), &json!({"value": 5}));
    let e = error(&run(&python, "import sys\nsys.exit(3)").await);
    assert!(e.starts_with("SystemExit: 3 (line 2)"), "{e}");
    let e = error(&run(&python, "raise KeyboardInterrupt").await);
    assert!(e.starts_with("KeyboardInterrupt (line 1)"), "{e}");
}

#[tokio::test]
async fn cannot_escape_the_sandbox() {
    let python = python().await;
    let code = r#"
import os, socket, subprocess, threading

def fails(f):
    try:
        f()
    except Exception as e:
        return type(e).__name__
    return False

def write_stdlib():
    with open("/lib/python3.15/x.py", "w") as f:
        f.write("x = 1")

{
    "read_stdlib": open("/lib/python3.15/os.py").read(6),
    "write_stdlib": fails(write_stdlib),
    "append_stdlib": fails(lambda: open("/lib/python3.15/os.py", "a")),
    "mkdir": fails(lambda: os.mkdir("/lib/x")),
    "remove": fails(lambda: os.remove("/lib/python3.15/os.py")),
    "list_root": fails(lambda: os.listdir("/")),
    "parent": fails(lambda: os.listdir("/lib/..")),
    "climb": fails(lambda: open("/lib/../../../../etc/passwd").read()),
    "etc": fails(lambda: open("/etc/passwd").read()),
    "tmp": fails(lambda: open("/tmp/x", "w")),
    "socket": fails(lambda: socket.socket()),
    "connect": fails(lambda: socket.create_connection(("127.0.0.1", 80))),
    "subprocess": fails(lambda: subprocess.run(["ls"])),
    "system": hasattr(os, "system"),
    "thread": fails(lambda: threading.Thread(target=print).start()),
    "environ": dict(os.environ),
}
"#;
    let out = run(&python, code).await;
    let v = &value(&out)["value"];
    assert_eq!(v["read_stdlib"], "r\"\"\"OS", "{v}");
    for denied in [
        "write_stdlib",
        "append_stdlib",
        "mkdir",
        "remove",
        "list_root",
        "parent",
        "climb",
        "etc",
        "tmp",
        "socket",
        "connect",
        "subprocess",
        "thread",
    ] {
        assert!(v[denied].is_string(), "{denied} did not fail: {v}");
    }
    assert_eq!(v["system"], false, "{v}");
    assert_eq!(v["environ"], json!({}), "{v}");
    // Nothing was written.
    assert!(!python_dir().join("lib/python3.15/x.py").exists());
}

#[tokio::test]
async fn no_numpy() {
    let python = python().await;
    let out = run(&python, "import numpy").await;
    let e = error(&out);
    assert!(
        e.starts_with("ModuleNotFoundError: No module named 'numpy' (line 1)"),
        "{e}"
    );
    assert!(out.next.unwrap().contains("statistics"));
}

fn small_budget() -> Limits {
    Limits {
        call_timeout: Duration::from_secs(30),
        cpu_ticks: 10,
        memory_bytes: 256 << 20,
        ..Limits::default()
    }
}

#[tokio::test]
async fn infinite_loop_is_stopped() {
    let python = python().await.with_limits(small_budget());
    let started = Instant::now();
    let e = error(&run(&python, "while True: pass").await);
    assert!(e.contains("compute budget"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(10), "{e}");
}

#[tokio::test]
async fn wall_clock_is_capped() {
    let python = python().await.with_limits(Limits {
        call_timeout: Duration::from_millis(500),
        ..Limits::default()
    });
    let e = error(&run(&python, "import time\ntime.sleep(60)").await);
    assert!(e.contains("timed out"), "{e}");
}

#[tokio::test]
async fn memory_bomb_is_a_memory_error() {
    let python = python().await;
    let out = run(&python, "b'x' * 2**30").await;
    let e = error(&out);
    assert!(e.starts_with("MemoryError (line 1)"), "{e}");
    assert!(out.next.unwrap().contains("256 MiB"));
    // Well over the components' 64 MiB still fits.
    let out = run(&python, "len(b'x' * (128 << 20))").await;
    assert_eq!(value(&out), &json!({"value": 128 << 20}));
}

#[tokio::test]
async fn output_flood_is_capped() {
    let python = python().await;
    let out = run(&python, "print('x' * 10_000_000)\n1").await;
    let v = value(&out);
    assert_eq!(v["value"], 1);
    assert_eq!(v["stdout"].as_str().unwrap().len(), 16 * 1024);
    assert_eq!(v["stdout_truncated"], true);
    let out = run(&python, "'x' * 10_000_000").await;
    let e = error(&out);
    assert!(e.contains("over the limit"), "{e}");
    assert!(out.next.unwrap().contains("aggregate"));
    // Writing to the real standard output fills the host's pipe, and the reply is lost.
    let e = error(&run(&python, "import os\nos.write(1, b'x' * 10_000_000)").await);
    assert!(e.contains("output over 64 KiB"), "{e}");
}

#[tokio::test]
async fn next_run_is_clean_after_a_failure() {
    let python = python().await.with_limits(small_budget());
    let e = error(
        &run(
            &python,
            "import json\njson.leak = 1\nsecret = 1\nwhile True: pass",
        )
        .await,
    );
    assert!(e.contains("compute budget"), "{e}");
    let e = error(&run(&python, "secret").await);
    assert!(e.starts_with("NameError"), "{e}");
    let out = run(&python, "import json\nhasattr(json, 'leak')").await;
    assert_eq!(value(&out), &json!({"value": false}));
}

#[tokio::test]
async fn deep_recursion_is_an_error() {
    let python = python().await;
    let e = error(&run(&python, "def f(n):\n    return f(n + 1)\nf(0)").await);
    assert!(e.starts_with("RecursionError"), "{e}");
    let out = run(&python, "1").await;
    assert_eq!(value(&out), &json!({"value": 1}));
}

#[tokio::test]
async fn refuses_an_unapproved_python_wasm() {
    // Under TMPDIR, never the system's temporary directory.
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = std::fs::read(python_dir().join("python.wasm")).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(dir.path().join("python.wasm"), &bytes).unwrap();
    std::fs::create_dir_all(dir.path().join("lib/python3.15")).unwrap();
    let host = ComponentHost::new().unwrap();
    let e = Python::load(&host, dir.path()).await.err().unwrap();
    let e = format!("{e:#}");
    assert!(
        e.contains("is not the approved CPython 3.15.2 build"),
        "{e}"
    );
    let sha256 = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert!(e.contains(&sha256), "{e}");
    // The pinned build loads from the same kind of directory.
    assert!(Python::load(&host, &python_dir()).await.is_ok());
}

#[tokio::test]
async fn refuses_a_directory_without_the_standard_library() {
    let dir = tempfile::tempdir().unwrap();
    let host = ComponentHost::new().unwrap();
    let e = Python::load(&host, dir.path()).await.err().unwrap();
    assert!(format!("{e:#}").contains("mise run python"), "{e:#}");
}

#[tokio::test]
async fn runs_concurrently() {
    let python = python().await;
    let (a, b, c, d) = tokio::join!(
        run(&python, "sum(range(1000))"),
        run(&python, "2**10"),
        run(&python, "'x' * 3"),
        run(&python, "import math\nmath.factorial(5)"),
    );
    assert_eq!(value(&a), &json!({"value": 499500}));
    assert_eq!(value(&b), &json!({"value": 1024}));
    assert_eq!(value(&c), &json!({"value": "xxx"}));
    assert_eq!(value(&d), &json!({"value": 120}));
}
