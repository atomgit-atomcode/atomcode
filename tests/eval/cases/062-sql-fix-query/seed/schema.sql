CREATE TABLE products (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE sales (
    id         INTEGER PRIMARY KEY,
    product_id INTEGER NOT NULL REFERENCES products(id),
    qty        INTEGER NOT NULL
);
