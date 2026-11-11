"""Initialize the cl-base PostgreSQL database from scripts bundled in the NP4 JAR."""

import json
import os
import re
import secrets
import shutil
import subprocess
import sys
import uuid
import zipfile


SQL_ENTRIES = (
    "BOOT-INF/classes/init/init.sql",
    "BOOT-INF/classes/init/v1data.sql",
)
DATABASE = "cl-base"
NAMESPACE = "cangling"


def report(percent, message):
    print(f"PROGRESS\t{percent}\t{message}", flush=True)


def inspect(jar):
    if not os.path.isfile(jar):
        return {"available": False, "reason": f"未找到 {jar}"}
    try:
        with zipfile.ZipFile(jar) as archive:
            names = set(archive.namelist())
    except (OSError, zipfile.BadZipFile) as error:
        return {"available": False, "reason": f"无法读取 JAR：{error}"}
    missing = [name.rsplit("/", 1)[-1] for name in SQL_ENTRIES if name not in names]
    if missing:
        return {
            "available": False,
            "reason": f"JAR 缺少 PostgreSQL 初始化脚本：{', '.join(missing)}",
        }
    return {"available": True, "reason": ""}


def postgres_target():
    kubectl = kubectl_command()
    pods = json.loads(run_command(kubectl + ["-n", NAMESPACE, "get", "pods", "-l", "app=cis-postgis", "-o", "json"]))
    pod = next((item["metadata"]["name"] for item in pods["items"] if item["status"]["phase"] == "Running"), None)
    if not pod:
        raise RuntimeError("cis-postgis 没有运行中的 Pod")
    return kubectl, pod


def database_exists(kubectl, pod):
    return bool(run_command(psql(kubectl, pod, "postgres", "SELECT 1 FROM pg_database WHERE datname='cl-base'")))


def initialized(kubectl, pod):
    return database_exists(kubectl, pod) and run_command(
        psql(kubectl, pod, DATABASE, "SELECT to_regclass('public.lin_server_setting') IS NOT NULL")
    ) == "t"


def check_status(jar):
    try:
        kubectl, pod = postgres_target()
        if initialized(kubectl, pod):
            return {"available": False, "initialized": True, "reason": "lin_server_setting 表已存在，无需初始化"}
    except Exception as error:
        return {"available": False, "initialized": False, "reason": str(error)}
    return {**inspect(jar), "initialized": False}


def repair_duplicate_channel_tokens(sql):
    """Keep all seed channels while satisfying minipro_channel's unique token index."""
    seen = set()
    repaired = 0
    lines = []
    for line in sql.splitlines(keepends=True):
        if re.match(r'\s*INSERT\s+INTO\s+"public"\."minipro_channel"\s*\(', line, re.IGNORECASE):
            match = re.match(
                r'\s*INSERT\s+INTO\s+"public"\."minipro_channel"\s*\(([^)]*)\)\s*VALUES\s*\(',
                line, re.IGNORECASE,
            )
            if not match:
                raise RuntimeError("minipro_channel 的数据格式不符合预期")
            columns = re.findall(r'"([^"]+)"', match.group(1))
            token_index = columns.index("api_token")
            values = []
            quoted = False
            start = match.end()
            offset = start
            while offset < len(line):
                char = line[offset]
                if char == "'":
                    if quoted and offset + 1 < len(line) and line[offset + 1] == "'":
                        offset += 2
                        continue
                    quoted = not quoted
                elif char == "," and not quoted:
                    values.append((start, offset))
                    start = offset + 1
                elif char == ")" and not quoted:
                    values.append((start, offset))
                    break
                offset += 1
            if len(values) != len(columns):
                raise RuntimeError("minipro_channel 的字段和值数量不一致")
            token_start, token_end = values[token_index]
            token = line[token_start:token_end].strip()
            if not (token.startswith("'") and token.endswith("'")):
                raise RuntimeError("minipro_channel 的 API 令牌格式不符合预期")
            if token in seen:
                replacement = secrets.token_hex(16)
                line = line[:token_start] + " '" + replacement + "'" + line[token_end:]
                repaired += 1
            else:
                seen.add(token)
        lines.append(line)
    return "".join(lines), repaired


def kubectl_command():
    if shutil.which("k3s"):
        return ["k3s", "kubectl"]
    if shutil.which("kubectl"):
        return ["kubectl"]
    raise RuntimeError("当前主机没有 k3s 或 kubectl")


def run_command(args, *, data=None):
    result = subprocess.run(args, input=data, capture_output=True, timeout=180)
    if result.returncode:
        error = result.stderr.decode("utf-8", errors="replace")
        safe = [line for line in error.splitlines() if "DETAIL:" not in line]
        raise RuntimeError("\n".join(safe[-8:]) or f"命令退出码 {result.returncode}")
    return result.stdout.decode("utf-8", errors="replace").strip()


def psql(kubectl, pod, database, sql):
    shell = (
        'export PGPASSWORD="$POSTGRES_PASS"; '
        'psql -h 127.0.0.1 -U "$POSTGRES_USER" '
        f'-d "{database}" -X -v ON_ERROR_STOP=1 -Atc "$1"'
    )
    return kubectl + ["-n", NAMESPACE, "exec", pod, "--", "sh", "-ec", shell, "base-init", sql]


def sql_file(kubectl, pod, database, contents):
    path = "/tmp/cangling-base-init-" + uuid.uuid4().hex + ".sql"
    try:
        run_command(
            kubectl + ["-n", NAMESPACE, "exec", "-i", pod, "--", "sh", "-ec", 'umask 077; cat > "$1"', "base-init", path],
            data=contents,
        )
        shell = (
            'export PGPASSWORD="$POSTGRES_PASS"; '
            'psql -h 127.0.0.1 -U "$POSTGRES_USER" '
            f'-d "{database}" -X -q -o /dev/null -v ON_ERROR_STOP=1 -1 -f "$1"'
        )
        return run_command(
            kubectl + ["-n", NAMESPACE, "exec", pod, "--", "sh", "-ec", shell, "base-init", path]
        )
    finally:
        run_command(kubectl + ["-n", NAMESPACE, "exec", pod, "--", "rm", "-f", path])


def database_command(kubectl, pod, program, database):
    shell = (
        'export PGPASSWORD="$POSTGRES_PASS"; '
        f'{program} -h 127.0.0.1 -U "$POSTGRES_USER" '
        + ('-O "$POSTGRES_USER" ' if program == 'createdb' else '')
        + '"$1"'
    )
    return kubectl + ["-n", NAMESPACE, "exec", pod, "--", "sh", "-ec", shell, "base-init", database]


def run(jar):
    kubectl, pod = postgres_target()
    if initialized(kubectl, pod):
        print("RESULT\tlin_server_setting 表已存在，已跳过 base 初始化。", flush=True)
        return
    result = inspect(jar)
    if not result["available"]:
        raise RuntimeError(result["reason"])
    report(5, "从 JAR 提取 init.sql 和 v1data.sql")
    with zipfile.ZipFile(jar) as archive:
        schema, seed = [archive.read(name) for name in SQL_ENTRIES]
    schema_text = schema.decode("utf-8-sig")
    preamble = re.compile(
        r'\A\s*DROP DATABASE IF EXISTS "cl-base";\s*'
        r'CREATE DATABASE "cl-base" owner "cangling";\s*'
        r'CREATE EXTENSION postgis;\s*',
        re.IGNORECASE,
    )
    match = preamble.match(schema_text)
    if not match:
        raise RuntimeError("init.sql 的数据库创建前导语句不符合预期，已拒绝执行")
    schema_text = schema_text[match.end():]
    schema_text, begin_count = re.subn(r'(?im)^\s*BEGIN\s*;\s*$', '', schema_text)
    schema_text, commit_count = re.subn(r'(?im)^\s*COMMIT\s*;\s*$', '', schema_text)
    if (begin_count, commit_count) != (1, 1):
        raise RuntimeError("init.sql 的事务语句不符合预期，已拒绝执行")
    schema = schema_text.encode("utf-8")
    seed_text = seed.decode("utf-8-sig")
    channel_marker = "-- np4首页渠道"
    if channel_marker in seed_text:
        sequence_sync = (
            "SELECT setval('public.seq_minipro_channel', "
            "GREATEST(COALESCE((SELECT max(id) FROM public.minipro_channel), 0), 1), true);\n"
        )
        seed_text = seed_text.replace(channel_marker, sequence_sync + channel_marker, 1)
    seed_text, repaired = repair_duplicate_channel_tokens(seed_text)
    if repaired:
        report(10, f"已为 {repaired} 条重复渠道记录生成独立 API 令牌")
    seed = seed_text.encode("utf-8")
    for name, content in zip(SQL_ENTRIES, (schema, seed)):
        text = content.decode("utf-8-sig")
        if re.search(r"(?im)^\s*(DROP|CREATE)\s+DATABASE\b|^\s*DROP\s+SCHEMA\b|^\s*TRUNCATE\b|^\s*DELETE\s+FROM\b", text):
            raise RuntimeError(f"{name} 包含删库或清空数据语句，已拒绝执行")
        if "AUTO_INCREMENT" in text or "SET FOREIGN_KEY_CHECKS" in text:
            raise RuntimeError(f"{name} 不是 PostgreSQL 脚本")

    report(15, f"检查 {pod} 中的 {DATABASE} 数据库")
    exists = database_exists(kubectl, pod)
    postgis_schema = "public"
    if exists:
        tables = run_command(psql(kubectl, pod, DATABASE, "SELECT count(*) FROM pg_tables WHERE schemaname='public' AND tablename<>'spatial_ref_sys'"))
        if int(tables) != 0:
            raise RuntimeError(f"{DATABASE} 已有 {tables} 张业务表；为防止覆盖现有数据，初始化已停止")
        configured_schema = run_command(psql(kubectl, pod, DATABASE, "SELECT extnamespace::regnamespace::text FROM pg_extension WHERE extname='postgis'"))
        if configured_schema:
            if not re.fullmatch(r"[a-zA-Z_][a-zA-Z_0-9]*", configured_schema):
                raise RuntimeError("PostGIS schema 名称不符合预期")
            postgis_schema = configured_schema
    if postgis_schema != "public":
        for object_name in ("geometry", "gist_geometry_ops_2d"):
            schema = schema.replace(
                f'"public"."{object_name}"'.encode("ascii"),
                f'"{postgis_schema}"."{object_name}"'.encode("ascii"),
            )
        schema = f'SET search_path TO "{postgis_schema}", public;\n'.encode("ascii") + schema

    report(20, "在隔离数据库验证建表和基础数据脚本")
    probe = "cangling_base_probe_" + uuid.uuid4().hex[:12]
    run_command(database_command(kubectl, pod, "createdb", probe))
    try:
        if postgis_schema != "public":
            run_command(psql(kubectl, pod, probe, f'CREATE SCHEMA "{postgis_schema}"'))
        run_command(psql(kubectl, pod, probe, f'CREATE EXTENSION IF NOT EXISTS postgis WITH SCHEMA "{postgis_schema}"'))
        sql_file(kubectl, pod, probe, schema + b"\n" + seed)
        probe_users = run_command(psql(kubectl, pod, probe, "SELECT count(*) FROM public.lin_user"))
        if int(probe_users) == 0:
            raise RuntimeError("隔离验证未产生基础用户记录")
    finally:
        run_command(database_command(kubectl, pod, "dropdb", probe))

    if sys.argv[1] == "probe":
        print(f"RESULT\t隔离验证通过：{probe_users} 条基础用户记录，正式数据库未修改。", flush=True)
        return

    if not exists:
        run_command(database_command(kubectl, pod, "createdb", DATABASE))
    run_command(psql(kubectl, pod, DATABASE, "CREATE EXTENSION IF NOT EXISTS postgis"))
    if initialized(kubectl, pod):
        print("RESULT\tlin_server_setting 表已存在，已跳过 base 初始化。", flush=True)
        return
    report(30, "正在事务中执行 init.sql 和 v1data.sql")
    sql_file(kubectl, pod, DATABASE, schema + b"\n" + seed)
    report(85, "建表及基础记录导入已完成")
    report(90, "正在核对初始化结果")
    count = run_command(psql(kubectl, pod, DATABASE, "SELECT count(*) FROM pg_tables WHERE schemaname='public' AND tablename<>'spatial_ref_sys'"))
    users = run_command(psql(kubectl, pod, DATABASE, "SELECT count(*) FROM public.lin_user"))
    if int(count) == 0 or int(users) == 0:
        raise RuntimeError(f"初始化后业务表 {count} 张，用户 {users} 条；结果不完整")
    print(f"RESULT\tbase 初始化完成：{DATABASE} 数据库中 {count} 张业务表、{users} 条用户记录。", flush=True)


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3 or sys.argv[1] not in ("check", "probe", "run"):
            raise RuntimeError("用法：base_init.py check|probe|run <jar-path>")
        if sys.argv[1] == "check":
            print(json.dumps(check_status(sys.argv[2]), ensure_ascii=False))
        else:
            run(sys.argv[2])
    except Exception as error:
        print(f"base 初始化失败：{error}", file=sys.stderr)
        sys.exit(1)
