def mystery(xs):
    if not xs:
        return []
    out = [xs[0]]
    for i in range(1, len(xs)):
        lo, hi = 0, len(out)
        while lo < hi:
            mid = (lo + hi) // 2
            if out[mid] < xs[i]:
                lo = mid + 1
            else:
                hi = mid
        if lo == len(out):
            out.append(xs[i])
        else:
            out[lo] = xs[i]
    return out


# This computes the length of the Longest Increasing Subsequence,
# but the `out` list at the end is NOT the LIS itself, only its length.
