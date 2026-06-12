[Terraform/HCL] This batch contains Terraform; pay extra attention to:

#### security
- Hardcoded secrets/credentials in `.tf` / `.tfvars`; overly permissive IAM (`*` actions/resources), `0.0.0.0/0` ingress
- Public storage buckets; unencrypted resources (encryption flags off)

#### state / correctness
- Resources that force-replace on change (data loss); missing `lifecycle` protection on stateful resources
- Unpinned provider/module versions; `count`/`for_each` index churn causing recreations

#### robustness
- Sensitive outputs not marked `sensitive`; missing variable validation/types; implicit dependencies needing `depends_on`
