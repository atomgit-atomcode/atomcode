def bsearch(arr, target):
    """Return index of target in sorted arr, or -1 if not found."""
    lo, hi = 0, len(arr)
    while lo < hi:
        mid = (lo + hi) // 2
        if arr[mid] == target:
            return mid
        if arr[mid] < target:
            lo = mid  # BUG: should be mid + 1
        else:
            hi = mid
    return -1
