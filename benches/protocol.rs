//! GAP protocol-layer throughput benchmarks (criterion).
//!
//! Measures the hot paths that bound node throughput:
//!   - Ed25519 signing / verification (every message)
//!   - contract lifecycle (propose -> accept -> deliver -> settle)
//!   - escrow operations (park/release/refund/rule instructions)
//!   - receipt chain append + verify
//!   - SQLite storage append + read (the audit spine)
//!
//! Run:  cargo bench --bench protocol
//! Quick: cargo bench --bench protocol -- --warm-up-time 1 --measurement-time 3

use criterion::{criterion_group, criterion_main, Criterion};
use gap::contract::{Contract, Terms};
use gap::identity::AgentIdentity;
use gap::message::{Envelope, Kind};
use gap::payment::Escrow;
use gap::receipt_chain::ChainLedger;
use gap::storage::sqlite::SqliteStorage;
use gap::storage::Storage;
use serde_json::json;
use std::hint::black_box;

fn terms() -> Terms {
    Terms {
        input: json!({ "text": "hello" }),
        deliverable: json!({ "summary": "world" }),
        acceptance_criteria: vec!["ok".into()],
        deadline: 4_000_000_000,
        price: gap::contract::Price {
            amount: 0.05,
            currency: "EUR".into(),
            model: "fixed".into(),
            cap: Some(100.0),
        },
        autonomy: "propose".into(),
        confidentiality: None,
    }
}

fn bench_identity(c: &mut Criterion) {
    let mut g = c.benchmark_group("identity");
    g.throughput(criterion::Throughput::Elements(1));
    g.bench_function("generate", |b| {
        b.iter(|| black_box(AgentIdentity::generate()))
    });
    let id = AgentIdentity::generate();
    let msg = b"GAP-benchmark-message-0123456789";
    let sig = id.sign(msg);
    g.bench_function("sign_32b", |b| b.iter(|| black_box(id.sign(msg))));
    g.bench_function("verify_32b", |b| b.iter(|| black_box(id.verify(msg, &sig))));
    g.finish();
}

fn bench_contract(c: &mut Criterion) {
    let mut g = c.benchmark_group("contract");
    let client = AgentIdentity::generate();
    let provider = AgentIdentity::generate();
    let t = terms();
    g.bench_function("new_draft", |b| {
        b.iter(|| {
            black_box(Contract::propose(
                &client,
                provider.did().clone(),
                "cap:p:analyze",
                t.clone(),
                true,
            ))
        })
    });
    let ctr = Contract::propose(&client, provider.did().clone(), "cap:p:analyze", t, true);
    g.bench_function("sign_and_accept", |b| {
        b.iter(|| {
            let c = Contract::propose(
                &client,
                provider.did().clone(),
                "cap:p:analyze",
                terms(),
                true,
            );
            black_box(c.accept_by_provider(&provider).ok())
        })
    });
    g.bench_function("state_serialize", |b| {
        b.iter(|| black_box(serde_json::to_string(&ctr).unwrap()))
    });
    g.finish();
}

fn bench_escrow(c: &mut Criterion) {
    let mut g = c.benchmark_group("escrow");
    let client = AgentIdentity::generate();
    let provider = AgentIdentity::generate();
    let node = AgentIdentity::generate();
    let ctr = Contract::propose(
        &client,
        provider.did().clone(),
        "cap:p:analyze",
        terms(),
        true,
    );
    let ctr = ctr.accept_by_provider(&provider).unwrap();
    let mut escrow = Escrow::new(node.clone());
    escrow.register(ctr.clone()).unwrap();

    let park = Envelope::new(
        client.did().clone(),
        node.did().clone(),
        Kind::PayPark,
        json!({ "amount": 0.05 }),
    )
    .for_contract(&ctr.contract_id)
    .sign(&client);
    g.bench_function("park_instruction", |b| {
        b.iter(|| {
            let e = Envelope::new(
                client.did().clone(),
                node.did().clone(),
                Kind::PayPark,
                json!({ "amount": 0.05 }),
            )
            .for_contract(&ctr.contract_id)
            .sign(&client);
            black_box(e)
        })
    });
    g.bench_function("verify_and_apply_park", |b| {
        b.iter(|| {
            let mut e = Escrow::new(node.clone());
            e.register(ctr.clone()).ok();
            e.park(&park).ok();
            black_box(e)
        })
    });
    g.finish();
}

fn bench_receipt_chain(c: &mut Criterion) {
    let mut g = c.benchmark_group("receipt_chain");
    let _id = AgentIdentity::generate();
    let _chain = ChainLedger::new("urn:gap:chain:bench");
    g.bench_function("append", |b| {
        b.iter(|| {
            let mut ch = ChainLedger::new("urn:gap:chain:bench");
            ch.append(json!({ "event": "work.done", "t": 1 }));
            black_box(ch)
        })
    });
    let chain2 = {
        let mut ch = ChainLedger::new("urn:gap:chain:bench");
        for _ in 0..1000 {
            ch.append(json!({ "event": "work.done" }));
        }
        ch
    };
    g.bench_function("verify_chain_1000", |b| {
        b.iter(|| black_box(chain2.verify_chain()))
    });
    g.finish();
}

fn bench_storage_sqlite(c: &mut Criterion) {
    let mut g = c.benchmark_group("storage_sqlite");
    let mut storage = SqliteStorage::open(":memory:").unwrap();
    g.bench_function("append_event", |b| {
        b.iter(|| {
            storage.append_event("work.done", json!({ "t": 1 })).ok();
            black_box(())
        })
    });
    // Pre-fill so reads hit a populated table.
    let mut storage2 = SqliteStorage::open(":memory:").unwrap();
    for i in 0..10_000 {
        storage2
            .append_event("work.done", json!({ "i": i }))
            .unwrap();
    }
    g.bench_function("read_events_100", |b| {
        b.iter(|| black_box(storage2.events_after(0, 100)))
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_identity,
    bench_contract,
    bench_escrow,
    bench_receipt_chain,
    bench_storage_sqlite
);
criterion_main!(benches);
