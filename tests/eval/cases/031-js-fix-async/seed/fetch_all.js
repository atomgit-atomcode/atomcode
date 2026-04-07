// Simulated "fetch": resolves to { url, ok } after delayMs,
// or rejects if url contains "fail".
function fakeFetch(url, delayMs) {
    return new Promise((resolve, reject) => {
        setTimeout(() => {
            if (url.includes("fail")) {
                reject(new Error(`fetch failed: ${url}`));
            } else {
                resolve({ url, ok: true });
            }
        }, delayMs);
    });
}

// BUG 1: awaits sequentially — loses concurrency.
// BUG 2: if one fetch rejects, the whole function hangs.
async function fetchAll(urls) {
    const results = [];
    for (const u of urls) {
        const r = await fakeFetch(u, 50);
        results.push(r);
    }
    return results;
}

module.exports = { fakeFetch, fetchAll };
