[HTML] This batch contains HTML; pay extra attention to:

#### security
- Unescaped/user-controlled content rendered (XSS); inline event handlers or inline scripts with dynamic data
- `target="_blank"` without `rel="noopener noreferrer"`; external resources without integrity / SRI

#### correctness / a11y
- Unclosed/mismatched tags, duplicate `id`; form inputs without labels / `name`; missing `alt` on images
- Links/buttons without accessible text; incorrect ARIA roles
