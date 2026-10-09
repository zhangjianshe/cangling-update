#!/usr/bin/env python3
"""Backfill Nginx User-Agent dimensions without duplicating traffic totals.

The script temporarily disables the cangling-update statistics scheduler, snapshots
the existing per-inode log cursors, rebuilds nginx_bucket_devices in a separate
SQLite work database, then swaps the rebuilt rows into the live database in one
transaction. Bytes appended after the cursor snapshot remain for the normal
incremental collector, so they are not lost or counted twice.
"""

from __future__ import annotations

import argparse
import gzip
import os
import re
import sqlite3
import sys
import time
from contextlib import closing
from datetime import datetime, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo


TIME_RE = re.compile(
    rb"\[(\d{2})/([A-Za-z]{3})/(\d{4}):(\d{2}):(\d{2}):\d{2} ([+-]\d{4})\]"
)
MONTHS = {
    b"Jan": 1,
    b"Feb": 2,
    b"Mar": 3,
    b"Apr": 4,
    b"May": 5,
    b"Jun": 6,
    b"Jul": 7,
    b"Aug": 8,
    b"Sep": 9,
    b"Oct": 10,
    b"Nov": 11,
    b"Dec": 12,
}
SHANGHAI = ZoneInfo("Asia/Shanghai")
STAGE_SCHEMA = """
CREATE TABLE IF NOT EXISTS devices (
  bucket_start TEXT NOT NULL, platform TEXT NOT NULL, os_version TEXT NOT NULL,
  device_model TEXT NOT NULL, network_type TEXT NOT NULL,
  client_app TEXT NOT NULL, app_version TEXT NOT NULL,
  requests INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(bucket_start, platform, os_version, device_model, network_type, client_app, app_version)
);
"""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="独立回填 Nginx 历史 User-Agent 终端统计，不重复累计流量数据"
    )
    parser.add_argument("--db", required=True, type=Path, help="cangling.db 路径")
    parser.add_argument("--log-dir", required=True, type=Path, help="Nginx 日志目录")
    parser.add_argument(
        "--work-db",
        type=Path,
        help="临时聚合数据库；默认位于 cangling.db 同目录",
    )
    parser.add_argument(
        "--rate-mbps",
        type=float,
        default=30.0,
        help="平均读取限速，MB/s；0 表示不限速，默认 30",
    )
    parser.add_argument(
        "--keep-work-db", action="store_true", help="成功后保留临时聚合数据库"
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="完成解析但不替换线上终端统计表"
    )
    return parser.parse_args()


def is_access_log(path: Path) -> bool:
    name = path.name.lower()
    if name.startswith("monitor_access.log"):
        return False
    return name == "access.log" or name.startswith("access.log-") or name.startswith(
        "access.log."
    )


def find_value(text: str, marker: str, endings: str) -> str:
    pos = text.find(marker)
    if pos < 0:
        return ""
    value = text[pos + len(marker) :]
    indexes = [value.find(char) for char in endings if value.find(char) >= 0]
    if indexes:
        value = value[: min(indexes)]
    return value.strip()


def classify_user_agent(user_agent: str) -> tuple[str, str, str, str, str, str]:
    ua = user_agent.strip()
    lower = ua.lower()
    platform, os_version, device_model = "其他", "", ""
    if not ua or ua == "-":
        platform = "未知"
    elif "android" in lower:
        platform = "Android"
        os_version = find_value(ua, "Android ", ";)")
        android = ua.find("Android ")
        separator = ua.find(";", android)
        if separator >= 0:
            candidate = ua[separator + 1 :].split(";", 1)[0].strip()
            candidate = candidate.split(" Build/", 1)[0].strip()
            if len(candidate) <= 80:
                device_model = candidate
    elif any(value in lower for value in ("iphone", "ipad", "ipod")):
        platform = "Apple iOS/iPadOS"
        device_model = "iPad" if "ipad" in lower else "iPod" if "ipod" in lower else "iPhone"
        os_version = (
            find_value(ua, "CPU iPhone OS ", " ")
            or find_value(ua, "CPU OS ", " ")
        ).replace("_", ".")
    elif "windows" in lower:
        platform = "Windows"
    elif "macintosh" in lower or "mac os x" in lower:
        platform = "macOS"
    elif "linux" in lower:
        platform = "Linux"

    network_type = find_value(ua, "NetType/", " ;) ").upper() or "未知"
    if "micromessenger/" in lower:
        client_app = "微信小程序" if "miniprogram" in lower else "微信"
        app_version = find_value(ua, "MicroMessenger/", "( ")
    else:
        client_app, app_version = "其他", ""
    return platform, os_version, device_model, network_type, client_app, app_version


def bucket_for(line: bytes, cache: dict[tuple[bytes, ...], str]) -> str | None:
    match = TIME_RE.search(line)
    if not match:
        return None
    key = match.groups()
    cached = cache.get(key)
    if cached:
        return cached
    day, month_name, year, hour, minute, offset_text = key
    month = MONTHS.get(month_name)
    if not month:
        return None
    sign = 1 if offset_text[:1] == b"+" else -1
    offset = sign * (int(offset_text[1:3]) * 60 + int(offset_text[3:5]))
    source = datetime(
        int(year), int(month), int(day), int(hour), int(minute),
        tzinfo=timezone(timedelta(minutes=offset)),
    )
    local = source.astimezone(SHANGHAI).replace(
        minute=0 if source.astimezone(SHANGHAI).minute < 30 else 30,
        second=0,
        microsecond=0,
    )
    value = local.isoformat(timespec="seconds")
    cache[key] = value
    return value


def snapshot_cursors(conn: sqlite3.Connection) -> dict[str, int]:
    rows = conn.execute(
        "SELECT identity, MAX(0, offset - length(pending)) FROM nginx_log_cursors"
    ).fetchall()
    return {str(identity): int(offset) for identity, offset in rows}


def snapshot_files(log_dir: Path, cursors: dict[str, int]) -> list[tuple[Path, int | None]]:
    files: list[tuple[Path, int | None]] = []
    for path in sorted(log_dir.iterdir()):
        if not path.is_file() or not is_access_log(path):
            continue
        if path.suffix.lower() == ".gz":
            files.append((path, None))
            continue
        stat = path.stat()
        identity = f"{stat.st_dev}:{stat.st_ino}"
        files.append((path, min(cursors.get(identity, stat.st_size), stat.st_size)))
    return files


def flush_counts(conn: sqlite3.Connection, counts: dict[tuple[str, ...], int]) -> None:
    if not counts:
        return
    conn.executemany(
        """
        INSERT INTO devices(bucket_start,platform,os_version,device_model,network_type,client_app,app_version,requests)
        VALUES(?,?,?,?,?,?,?,?)
        ON CONFLICT(bucket_start,platform,os_version,device_model,network_type,client_app,app_version)
        DO UPDATE SET requests=requests+excluded.requests
        """,
        ((*key, count) for key, count in counts.items()),
    )
    conn.commit()
    counts.clear()


def scan_file(
    path: Path,
    limit: int | None,
    stage: sqlite3.Connection,
    rate_mbps: float,
    totals: dict[str, int],
) -> None:
    opener = gzip.open if path.suffix.lower() == ".gz" else open
    counts: dict[tuple[str, ...], int] = {}
    time_cache: dict[tuple[bytes, ...], str] = {}
    started = time.monotonic()
    consumed = 0
    with opener(path, "rb") as stream:
        while True:
            before = consumed
            line = stream.readline()
            if not line:
                break
            consumed += len(line)
            if limit is not None and consumed > limit:
                break
            bucket = bucket_for(line, time_cache)
            quoted = line.split(b'"')
            if bucket is None or len(quoted) < 7:
                totals["skipped"] += 1
                continue
            user_agent = quoted[5].decode("utf-8", "replace")
            key = (bucket, *classify_user_agent(user_agent))
            counts[key] = counts.get(key, 0) + 1
            totals["parsed"] += 1
            if len(counts) >= 20_000:
                flush_counts(stage, counts)
            if rate_mbps > 0 and consumed - before > 0:
                expected = consumed / (rate_mbps * 1024 * 1024)
                delay = expected - (time.monotonic() - started)
                if delay > 0:
                    time.sleep(min(delay, 0.1))
    flush_counts(stage, counts)
    totals["bytes"] += consumed


def prepare_live(conn: sqlite3.Connection) -> int:
    conn.execute("PRAGMA busy_timeout=30000")
    row = conn.execute(
        "SELECT enabled FROM nginx_stats_settings WHERE id=1"
    ).fetchone()
    if row is None:
        raise RuntimeError("nginx_stats_settings 不存在，请先启用 Nginx 流量统计")
    previous = int(row[0])
    conn.execute("BEGIN IMMEDIATE")
    conn.execute(
        "UPDATE nginx_stats_settings SET enabled=0,last_status='device_backfill',"
        "last_message='正在回填历史终端信息' WHERE id=1"
    )
    conn.commit()
    return previous


def remove_work_db(path: Path) -> None:
    for candidate in (path, Path(str(path) + "-wal"), Path(str(path) + "-shm")):
        candidate.unlink(missing_ok=True)


def restore_enabled(conn: sqlite3.Connection, enabled: int, message: str) -> None:
    conn.execute("BEGIN IMMEDIATE")
    conn.execute(
        "UPDATE nginx_stats_settings SET enabled=?,last_status='ok',last_message=? WHERE id=1",
        (enabled, message),
    )
    conn.commit()


def replace_live(conn: sqlite3.Connection, work_db: Path, enabled: int, totals: dict[str, int]) -> None:
    conn.execute("ATTACH DATABASE ? AS backfill", (str(work_db),))
    try:
        conn.execute("BEGIN IMMEDIATE")
        conn.execute("DELETE FROM nginx_bucket_devices")
        conn.execute(
            """
            INSERT INTO nginx_bucket_devices(
              bucket_start,platform,os_version,device_model,network_type,client_app,app_version,requests
            )
            SELECT bucket_start,platform,os_version,device_model,network_type,client_app,app_version,requests
            FROM backfill.devices
            """
        )
        conn.execute(
            "UPDATE nginx_stats_settings SET enabled=?,last_status='ok',last_message=? WHERE id=1",
            (enabled, f"历史终端信息回填完成：{totals['parsed']} 条"),
        )
        conn.commit()
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.execute("DETACH DATABASE backfill")


def main() -> int:
    args = parse_args()
    db_path = args.db.resolve()
    log_dir = args.log_dir.resolve()
    if not db_path.is_file():
        raise SystemExit(f"数据库不存在：{db_path}")
    if not log_dir.is_dir():
        raise SystemExit(f"日志目录不存在：{log_dir}")
    work_db = (args.work_db or db_path.with_name(db_path.name + ".device-backfill")).resolve()
    totals = {"parsed": 0, "skipped": 0, "bytes": 0}
    previous_enabled: int | None = None
    replaced = False

    with closing(sqlite3.connect(db_path, timeout=30)) as live:
        try:
            previous_enabled = prepare_live(live)
            cursors = snapshot_cursors(live)
            files = snapshot_files(log_dir, cursors)
            print(f"自动采集已暂停；快照到 {len(files)} 个日志文件。", flush=True)
            remove_work_db(work_db)
            with closing(sqlite3.connect(work_db)) as stage:
                stage.executescript(STAGE_SCHEMA)
                stage.execute("PRAGMA journal_mode=WAL")
                stage.execute("PRAGMA synchronous=NORMAL")
                for index, (path, limit) in enumerate(files, 1):
                    label = "完整 gzip" if limit is None else f"前 {limit} 字节"
                    print(f"[{index}/{len(files)}] {path.name}（{label}）", flush=True)
                    scan_file(path, limit, stage, max(0.0, args.rate_mbps), totals)
                    print(
                        f"  累计解析 {totals['parsed']:,} 条，读取 {totals['bytes'] / 1024**3:.2f} GiB",
                        flush=True,
                    )
                check = stage.execute("PRAGMA integrity_check").fetchone()[0]
                if check != "ok":
                    raise RuntimeError(f"临时数据库完整性检查失败：{check}")
            if args.dry_run:
                restore_enabled(live, previous_enabled, "历史终端信息回填 dry-run 完成，未替换数据")
                replaced = True
            else:
                replace_live(live, work_db, previous_enabled, totals)
                replaced = True
        finally:
            if previous_enabled is not None and not replaced:
                try:
                    restore_enabled(live, previous_enabled, "历史终端信息回填未完成，已恢复自动采集")
                except Exception as error:
                    print(f"警告：恢复自动采集失败：{error}", file=sys.stderr)

    print(
        f"完成：解析 {totals['parsed']:,} 条，跳过 {totals['skipped']:,} 条，"
        f"读取 {totals['bytes'] / 1024**3:.2f} GiB。",
        flush=True,
    )
    if not args.keep_work_db and not args.dry_run:
        remove_work_db(work_db)
    else:
        print(f"临时数据库：{work_db}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
