[MyBatis Mapper/DAO XML] This batch contains MyBatis mapping files; pay extra attention to:

#### Obvious Spelling Errors
- SQL keyword spelling errors
- Mapper interface method name mismatched with XML `id` attribute spelling
- Dynamic SQL tag attribute name spelling errors (e.g., field names in `test` conditions)

#### SQL Logic Errors
- **Condition errors**: wrong logical operators in WHERE conditions (AND/OR confusion)
- **JOIN condition errors**: join conditions using wrong fields or missing necessary join conditions
- **Dynamic SQL logic errors**: `<if test="">` condition judgment errors, such as null checks, type checks
- **SQL syntax errors**: missing commas, unmatched parentheses, and other obvious syntax errors

#### Critical Performance Issues
- **Full table scan risk**: missing WHERE conditions
- **Large queries without pagination**: may return large datasets without using LIMIT or pagination
- **Repeated subqueries**: same subquery used in multiple places; suggest extracting to temp table or optimizing SQL structure

#### SQL Injection Security Risks

**Real risks that should be reported:**
- **Direct string concatenation**: using `${}` to拼接user input parameters into SQL, causing injection risk
- **LIKE query concatenation**: directly concatenating LIKE conditions instead of safe parameter binding

**Cases that should NOT be reported:**
- **Correct use of `#{}` parameter binding**: MyBatis automatically escapes, safe
- **Static SQL**: fixed SQL without dynamic parameters

**Review principles:**
- Focus on critical issues that may cause data corruption, performance problems, or security risks
- Consider actual SQL execution efficiency and impact on database performance
- Prioritize identifying critical issues that may cause production failures
- Be cautious when context is unclear: when the full SQL execution context cannot be determined, prefer to skip rather than misreport
- Require sufficient evidence: only report when there is clear evidence of a problem
- Better to miss than to misreport: maintain high precision to avoid real issues being drowned out by large numbers of false positives
