-- BUG: this query double-counts and drops zero-sales products.
SELECT p.name AS product_name, s.qty AS total_qty
FROM products p
JOIN sales s ON s.product_id = p.id
ORDER BY s.qty DESC;
