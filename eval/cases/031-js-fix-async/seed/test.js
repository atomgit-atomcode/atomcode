const assert = require("assert");
const { fetchAll } = require("./fetch_all");

(async () => {
    // 1. Concurrent: 4 urls at 50ms each should finish well under 150ms total.
    const t0 = Date.now();
    const ok = await fetchAll(["a", "b", "c", "d"]);
    const elapsed = Date.now() - t0;
    assert.strictEqual(ok.length, 4, "should return 4 results");
    assert.ok(elapsed < 150, `should be concurrent, took ${elapsed}ms`);

    // 2. Failure: if any url fails, fetchAll must reject (not hang).
    let threw = false;
    try {
        await fetchAll(["a", "please-fail", "b"]);
    } catch (e) {
        threw = true;
    }
    assert.ok(threw, "fetchAll should reject on any failure");

    console.log("ALL TESTS PASSED");
})().catch((e) => {
    console.error("TEST FAILED:", e);
    process.exit(1);
});
