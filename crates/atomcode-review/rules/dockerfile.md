[Dockerfile] This batch contains Dockerfile(s); pay extra attention to:

#### security
- Running as root (missing `USER`); secrets baked into layers (`ARG`/`ENV` with credentials, `COPY` of secret files)
- `latest` / unpinned base image tags; `curl ... | sh` of unverified scripts; `ADD` of remote URLs (prefer `COPY`)
- `COPY . .` pulling in unwanted/secret files (missing `.dockerignore`)

#### correctness / size
- Cache invalidation order (`COPY` of source before dependency install); `apt-get install` without cleaning lists / `--no-install-recommends`
- Missing cleanup creating bloated layers; many `RUN` lines where one chained layer is intended

#### runtime
- Shell-form `CMD`/`ENTRYPOINT` swallowing signals (use exec / JSON-array form); missing `EXPOSE` / healthcheck where relevant; `WORKDIR` not set
