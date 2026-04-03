#!/usr/bin/env python3
import argparse
import json
import os
import ssl
import sys
from urllib import error, parse, request

REPO_OWNER = "bangxu"
REPO_NAME = "atomcode"
ACCESS_TOKEN = ""
API_HOST = "https://api.gitcode.com"
BODY_TEMPLATE = """
Release  Note

使用于mac和linux的当前最新版本，安装命令如下

```bash
# 进入 ~/.local/bin 目录
cd ~/.local/bin

# 通过 Finder 打开目录
open .

# 将下载后的 atomcode-xxx 文件放入到目录，重命名为 atomcode

# 设置 atomcode 的运行权限
chmod +x atomcode
```

最后，在终端中输入 atomcode 并运行即可
"""



def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Create a GitCode release, then fetch the attachment upload URL."
    )
    parser.add_argument(
        "--owner",
        default=REPO_OWNER,
        help="Repository owner or namespace, optional",
    )
    parser.add_argument(
        "--repo",
        default=REPO_NAME,
        help="Repository name, optional",
    )
    parser.add_argument(
        "--access-token",
        default=ACCESS_TOKEN,
        help="GitCode access token, optional",
    )
    parser.add_argument("--tag-name", required=True, help="Tag name to create")
    parser.add_argument("--body", default="", help="Release description, optional")
    parser.add_argument(
        "--file-name",
        required=True,
        help="Attachment file name used to fetch the upload URL",
    )
    parser.add_argument(
        "--file-path",
        required=True,
        help="Local file path to upload",
    )
    parser.add_argument(
        "--insecure",
        action="store_true",
        help="Retained for compatibility; SSL verification is already skipped by default",
    )
    return parser


def send_request(url: str, method: str = "GET", payload: dict | None = None) -> dict:
    data = None
    headers = {}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"

    req = request.Request(
        url=url,
        data=data,
        headers=headers,
        method=method,
    )
    ssl_context = ssl._create_unverified_context()

    with request.urlopen(req, timeout=30, context=ssl_context) as resp:
        body = resp.read().decode("utf-8")
        return json.loads(body) if body else {}


def create_tag_release(args: argparse.Namespace) -> dict:
    base_url = f"{API_HOST}/api/v5/repos/{args.owner}/{args.repo}/releases"
    url = f"{base_url}?access_token={parse.quote(args.access_token)}"
    body = args.body or BODY_TEMPLATE.replace("atomcode-v2.3.x", args.tag_name)

    payload = {
        "tag_name": args.tag_name,
        "name": args.tag_name,
        "body": body,
    }
    return send_request(url, method="POST", payload=payload)


def get_release_upload_url(args: argparse.Namespace) -> dict:
    url = (
        f"{API_HOST}/api/v5/repos/{args.owner}/{args.repo}/releases/"
        f"{parse.quote(args.tag_name)}/upload_url"
        f"?access_token={parse.quote(args.access_token)}"
        f"&file_name={parse.quote(args.file_name)}"
    )
    return send_request(url)


def upload_release_asset(upload_url_result: dict, file_path: str) -> dict:
    upload_url = upload_url_result.get("url")
    upload_headers = upload_url_result.get("headers", {})
    if not upload_url:
        raise ValueError("Missing upload URL in upload_url response.")

    with open(file_path, "rb") as file_obj:
        file_data = file_obj.read()

    req = request.Request(
        url=upload_url,
        data=file_data,
        headers=upload_headers,
        method="PUT",
    )
    ssl_context = ssl._create_unverified_context()

    with request.urlopen(req, timeout=60, context=ssl_context) as resp:
        response_body = resp.read().decode("utf-8", errors="replace").strip()
        return {
            "status_code": resp.status,
            "body": response_body or "success",
        }


def is_release_already_exists(exc: error.HTTPError, details: str) -> bool:
    if exc.code not in {400, 409}:
        return False

    try:
        payload = json.loads(details) if details else {}
    except json.JSONDecodeError:
        payload = {}

    error_message = str(payload.get("error_message", ""))
    return "Release already exists" in error_message


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    if not args.access_token:
        print(
            "Missing access token. Set ACCESS_TOKEN in the script or pass --access-token.",
            file=sys.stderr,
        )
        return 1
    if not os.path.isfile(args.file_path):
        print(f"File not found: {args.file_path}", file=sys.stderr)
        return 1

    try:
        release_result = create_tag_release(args)
    except error.HTTPError as exc:
        details = exc.read().decode("utf-8", errors="replace")
        if is_release_already_exists(exc, details):
            release_result = {
                "skipped": True,
                "reason": "release already exists",
                "details": json.loads(details),
            }
        else:
            print(f"HTTP {exc.code}: {details}", file=sys.stderr)
            return 1
    except error.URLError as exc:
        print(f"Request failed: {exc}", file=sys.stderr)
        return 1

    try:
        upload_url_result = get_release_upload_url(args)
        upload_result = upload_release_asset(upload_url_result, args.file_path)
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    except error.HTTPError as exc:
        details = exc.read().decode("utf-8", errors="replace")
        print(f"HTTP {exc.code}: {details}", file=sys.stderr)
        return 1
    except error.URLError as exc:
        print(f"Request failed: {exc}", file=sys.stderr)
        return 1

    print(
        json.dumps(
            {
                "release": release_result,
                "upload_url": upload_url_result,
                "upload_result": upload_result,
            },
            ensure_ascii=False,
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())