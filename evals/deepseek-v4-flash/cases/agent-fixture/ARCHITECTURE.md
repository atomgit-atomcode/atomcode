# Architecture

`Runtime` is the sole owner of mutable application state. `Driver` handles input
and display and must delegate state changes to Runtime. `legacy.py` is a one-way
data importer only and must never be called by production request handling.
