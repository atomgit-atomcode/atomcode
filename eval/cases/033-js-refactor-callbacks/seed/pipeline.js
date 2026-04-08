// Legacy callback-style pipeline. Refactor to async/await.
// Preserve the external behavior: loadData returns a numeric array,
// transform doubles every element, save just counts them.

function loadData(cb) {
    setTimeout(() => cb(null, [1, 2, 3, 4]), 10);
}

function transform(data, cb) {
    setTimeout(() => cb(null, data.map((x) => x * 2)), 10);
}

function save(data, cb) {
    setTimeout(() => cb(null, { saved: data.length }), 10);
}

function main() {
    loadData((err, data) => {
        if (err) {
            console.error("PIPELINE FAILED:", err.message);
            process.exit(1);
        }
        transform(data, (err, out) => {
            if (err) {
                console.error("PIPELINE FAILED:", err.message);
                process.exit(1);
            }
            save(out, (err, res) => {
                if (err) {
                    console.error("PIPELINE FAILED:", err.message);
                    process.exit(1);
                }
                if (res.saved === 4) {
                    console.log("PIPELINE OK");
                } else {
                    console.error("PIPELINE FAILED: wrong count");
                    process.exit(1);
                }
            });
        });
    });
}

main();
