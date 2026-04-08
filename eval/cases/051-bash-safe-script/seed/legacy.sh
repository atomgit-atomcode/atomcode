#!/usr/bin/env bash
# legacy cleanup script — unsafe

for f in $(ls $DATA_DIR/*.tmp); do
    echo deleting $f
    # rm $f   # would be dangerous; left commented
done
