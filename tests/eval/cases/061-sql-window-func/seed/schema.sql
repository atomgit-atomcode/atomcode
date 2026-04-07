CREATE TABLE txns (
    id      INTEGER PRIMARY KEY,
    account TEXT NOT NULL,
    ts      TEXT NOT NULL,
    amount  REAL NOT NULL
);
