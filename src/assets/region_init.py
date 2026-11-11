"""Initialize offline region data in cis-postgis without overwriting existing rows."""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import uuid
import zipfile

TARGETS = (
    ("sys_region_full_biz_cl.sql", "biz", "cl", "t_sys_region"),
    ("sys_region_full_base.sql", "cl-base", "public", "sys_region"),
)
COLUMNS = '"id","pid","deep","name","pinyin_prefix","pinyin","ext_id","ext_name","lng","lat","l","t","r","b","area","geom","profile"'


def report(percent, message):
    print(f"PROGRESS\t{percent}\t{message}", flush=True)


def command(args, **kwargs):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=3600, **kwargs)
    if result.returncode:
        # PostgreSQL can echo the huge geometry INSERT or row contents on failure.
        lines = [line[:500] for line in result.stderr.decode(errors="replace").splitlines()
                 if "DETAIL:" not in line and "INSERT INTO" not in line and "CONTEXT:" not in line]
        raise RuntimeError("\n".join(lines[-8:]) or f"命令退出码 {result.returncode}")
    return result.stdout.decode().strip()


def target():
    cli = ["k3s", "kubectl"] if shutil.which("k3s") else ["kubectl"]
    pods = json.loads(command(cli + ["-n", "cangling", "get", "pods", "-l", "app=cis-postgis", "-o", "json"]))
    for pod in pods["items"]:
        if pod["status"].get("phase") == "Running" and not pod["metadata"].get("deletionTimestamp"):
            return cli + ["-n", "cangling", "exec", "-i", pod["metadata"]["name"], "--"]
    raise RuntimeError("cis-postgis 没有运行中的 Pod")


def psql(remote, database, sql=None, stream=None):
    shell = ('export PGPASSWORD="$POSTGRES_PASS"; '
             'exec psql -h 127.0.0.1 -U "$POSTGRES_USER" -d "$1" '
             '-X -q -v ON_ERROR_STOP=1 -v VERBOSITY=terse ')
    if stream is None:
        return command(remote + ["sh", "-ec", shell + '-Atc "$2"', "region-init", database, sql])
    return command(remote + ["sh", "-ec", shell + '-1 -f -', "region-init", database], stdin=stream)


def pending_targets(remote):
    pending, completed = [], []
    for entry, database, schema, table in TARGETS:
        exists = psql(remote, "postgres", f"SELECT 1 FROM pg_database WHERE datname='{database}'")
        if not exists:
            raise RuntimeError(f"缺少 {database} 数据库，请先完成基础数据库初始化")
        relation = psql(remote, database, f"SELECT to_regclass('{schema}.{table}') IS NOT NULL")
        has_rows = relation == "t" and psql(remote, database, f'SELECT EXISTS(SELECT 1 FROM "{schema}"."{table}" LIMIT 1)') == "t"
        (completed if has_rows else pending).append((entry, database, schema, table))
    return pending, completed


def has_spatial_index(remote, database, schema, table):
    # A partial, expression, invalid or unfinished index cannot cover all rows.
    return psql(remote, database, f"""SELECT EXISTS (
      SELECT 1 FROM pg_index i
      JOIN pg_class c ON c.oid=i.indexrelid
      JOIN pg_am am ON am.oid=c.relam
      JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attname='geom'
      JOIN pg_opclass op ON op.oid=i.indclass[0]
      WHERE i.indrelid='{schema}.{table}'::regclass AND am.amname='gist'
        AND i.indisvalid AND i.indisready AND i.indpred IS NULL
        AND i.indexprs IS NULL AND i.indnkeyatts=1 AND i.indkey[0]=a.attnum
        AND op.opcname='gist_geometry_ops_2d'
    )""") == "t"


def postgis_schema(remote, database):
    name = psql(remote, database, "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='postgis'")
    if not name:
        raise RuntimeError(f"{database} 尚未安装 PostGIS 扩展")
    return '"' + name.replace('"', '""') + '"'


def ensure_spatial_index(remote, item):
    _, database, schema, table = item
    if has_spatial_index(remote, database, schema, table):
        return f"{database}.{schema}.{table} 空间索引已就绪"
    extension = postgis_schema(remote, database)
    name = f"{table}_geom_gist_idx"
    if psql(remote, database, f"SELECT to_regclass('{schema}.{name}') IS NOT NULL") == "t":
        name += "_" + uuid.uuid4().hex[:8]
    report(95, f"正在补建 {database}.{schema}.{table} 空间索引")
    # Concurrent creation permits normal writes and must run outside a transaction.
    psql(remote, database, f'CREATE INDEX CONCURRENTLY "{name}" ON "{schema}"."{table}" USING gist ("geom" {extension}."gist_geometry_ops_2d")')
    psql(remote, database, f'ANALYZE "{schema}"."{table}"')
    if not has_spatial_index(remote, database, schema, table):
        raise RuntimeError(f"{database}.{schema}.{table} 空间索引校验失败")
    return f"{database}.{schema}.{table} 空间索引已补建"


def inspect(archive):
    if not os.path.isfile(archive):
        raise RuntimeError(f"未找到行政区划数据包：{archive}")
    with zipfile.ZipFile(archive) as bundle:
        names = bundle.namelist()
        if len(names) != 2 or set(names) != {item[0] for item in TARGETS}:
            raise RuntimeError("行政区划压缩包须包含 biz 和 cl-base 两份约定的 SQL")
    checksum = archive + ".sha256"
    with open(checksum, encoding="ascii") as source:
        expected = source.read().split()[0]
    if len(expected) != 64 or any(c not in "0123456789abcdef" for c in expected):
        raise RuntimeError("行政区划 SHA-256 清单无效")
    return expected


def check(archive):
    inspect(archive)
    remote = target()
    pending, completed = pending_targets(remote)
    repairs = [f"{d}.{s}.{t}" for _, d, s, t in completed if not has_spatial_index(remote, d, s, t)]
    return {"available": bool(pending or repairs), "initialized": not pending and not repairs,
            "reason": "行政区划数据和空间索引已就绪" if not pending and not repairs else "",
            "indexRepairs": repairs,
            "pending": [f"{d}.{s}.{t}" for _, d, s, t in pending],
            "skipped": [f"{d}.{s}.{t}" for _, d, s, t in completed]}


def import_one(remote, archive, item, percent):
    entry, database, schema, table = item
    extension = postgis_schema(remote, database)
    stage = "region_init_" + uuid.uuid4().hex
    relation = f'"{schema}"."{table}"'
    sequence = f'"{schema}"."sys_region_id_seq"'
    report(percent, f"准备导入 {database}.{schema}.{table}")
    with tempfile.TemporaryFile() as output:
        def write(text):
            output.write(text.encode("utf-8"))
        write(f'SELECT pg_advisory_xact_lock(209182, 1401);\nCREATE SCHEMA IF NOT EXISTS "{schema}";\n')
        write(f'CREATE SCHEMA "{stage}";\nSET LOCAL search_path TO "{stage}", {extension}, public;\n')
        removed = {"begin": 0, "commit": 0, "table": 0, "sequence": 0}
        with zipfile.ZipFile(archive) as bundle, bundle.open(entry) as source:
            for raw in source:
                line = raw.decode("utf-8-sig").replace("\r\n", "\n")
                stripped = line.strip()
                if stripped in ("BEGIN;", "COMMIT;"):
                    removed["begin" if stripped == "BEGIN;" else "commit"] += 1
                    continue
                if stripped == f'DROP TABLE IF EXISTS "{table}";':
                    removed["table"] += 1
                    continue
                if stripped == 'DROP SEQUENCE IF EXISTS "sys_region_id_seq";':
                    removed["sequence"] += 1
                    continue
                line = line.replace("'cl.sys_region_id_seq'::regclass", f"'{stage}.sys_region_id_seq'::regclass")
                line = line.replace('"public"."gist_geometry_ops_2d"', f'{extension}."gist_geometry_ops_2d"')
                write(line)
        if removed != {"begin": 1, "commit": 1, "table": 1, "sequence": 1}:
            raise RuntimeError(f"{entry} 的建表和事务语句不符合预期")
        # The complete import and promotion share one transaction. Existing data
        # is checked again under the same lock before any business rows change.
        write(f'''\nDO $region$
DECLARE source_count bigint; target_count bigint;
BEGIN
  SELECT count(*) INTO source_count FROM "{stage}"."{table}";
  IF source_count = 0 THEN RAISE EXCEPTION '行政区划源数据为空'; END IF;
  IF to_regclass('{schema}.{table}') IS NOT NULL THEN
    LOCK TABLE {relation} IN ACCESS EXCLUSIVE MODE;
    SELECT count(*) INTO target_count FROM {relation};
    IF target_count > 0 THEN RAISE EXCEPTION '目标表已有行政区划数据，已停止导入'; END IF;
    INSERT INTO {relation} ({COLUMNS}) SELECT {COLUMNS} FROM "{stage}"."{table}";
  ELSE
    ALTER SEQUENCE "{stage}"."sys_region_id_seq" SET SCHEMA "{schema}";
    ALTER TABLE "{stage}"."{table}" SET SCHEMA "{schema}";
  END IF;
  SELECT count(*) INTO target_count FROM {relation};
  IF source_count <> target_count THEN RAISE EXCEPTION '行政区划记录数校验失败'; END IF;
  IF to_regclass('{schema}.sys_region_id_seq') IS NOT NULL THEN
    PERFORM setval('{sequence}', GREATEST((SELECT max(id) FROM {relation}), 1), true);
  END IF;
END $region$;
DROP SCHEMA "{stage}" CASCADE;
ANALYZE {relation};
''')
        output.seek(0)
        report(percent + 5, f"正在事务中写入 {database}.{schema}.{table}，请稍候")
        psql(remote, database, stream=output)
    result = psql(remote, database, f"SELECT count(*) || ' 条记录，' || count(geom) || ' 条空间几何，SRID ' || min({extension}.ST_SRID(geom)) FROM {relation}")
    report(percent + 40, f"{database}.{schema}.{table}：{result}")
    return f"{database}.{schema}.{table}：{result}"


def run(archive):
    expected = inspect(archive)
    remote = target()
    pending, completed = pending_targets(remote)
    summaries = [f"{d}.{s}.{t} 已有数据，跳过" for _, d, s, t in completed]
    if pending:
        report(1, "正在校验行政区划数据包 SHA-256")
        digest = hashlib.sha256()
        with open(archive, "rb") as source:
            for chunk in iter(lambda: source.read(8 * 1024 * 1024), b""):
                digest.update(chunk)
        if digest.hexdigest() != expected:
            raise RuntimeError("行政区划数据包 SHA-256 不匹配")
        for index, item in enumerate(pending):
            summaries.append(import_one(remote, archive, item, 5 + index * 45))
    for item in TARGETS:
        summaries.append(ensure_spatial_index(remote, item))
    print("RESULT\t" + "；".join(summaries), flush=True)


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3 or sys.argv[1] not in ("check", "run"):
            raise RuntimeError("用法：region_init.py check|run <zip-path>")
        if sys.argv[1] == "check":
            try:
                result = check(sys.argv[2])
            except Exception as error:
                result = {"available": False, "reason": str(error)}
            print(json.dumps(result, ensure_ascii=False))
        else:
            run(sys.argv[2])
    except Exception as error:
        print(f"行政区划初始化失败：{error}", file=sys.stderr)
        sys.exit(1)
