#!/usr/bin/env python3
"""Recover standard XMP packets from a Lightroom .lrcat SQLite catalog.

The catalog is opened read-only and immutable. No Adobe application or libraries are used.
The XMP packet is copied unchanged; Lightroom's proprietary develop history is not exported.

Choose exactly one destination:
  --output-dir DIR  write into a separate tree mirroring catalog folders
  --in-place        write missing <photo-stem>.xmp files beside reachable originals

Both modes are dry-run unless --write is supplied. Existing sidecars are never overwritten.
"""

from __future__ import annotations

import argparse
import csv
import os
import re
import sqlite3
import sys
import zlib
from pathlib import Path, PureWindowsPath

CRS_NS = b"http://ns.adobe.com/camera-raw-settings/1.0/"


class CatalogError(Exception):
    pass


def columns(db: sqlite3.Connection, table: str) -> set[str]:
    return {row[1] for row in db.execute(f'PRAGMA table_info("{table}")')}


def decode_packet(value: bytes | str | None) -> bytes | None:
    if value is None:
        return None
    if isinstance(value, str):
        data = value.encode("utf-8")
        if data.lstrip().startswith(b"<"):
            return data
        packed = data
    else:
        packed = bytes(value)
        if packed.lstrip().startswith(b"<"):
            return packed

    # Newer catalogs prefix zlib data with the uncompressed size as a big-endian u32.
    if len(packed) > 4:
        try:
            decoded = zlib.decompress(packed[4:])
            expected = int.from_bytes(packed[:4], "big")
            if expected == len(decoded):
                return decoded
        except zlib.error:
            pass
    # Also accept ordinary zlib packets for catalog variants that omit the length prefix.
    try:
        decoded = zlib.decompress(packed)
        if decoded.lstrip().startswith(b"<"):
            return decoded
    except zlib.error:
        pass
    raise CatalogError("an XMP packet used an unsupported encoding")


def path_parts(relative: str) -> list[str]:
    # Catalog folder paths are relative, but validate instead of trusting database contents.
    normalized = relative.replace("\\", "/")
    if normalized.startswith("/") or PureWindowsPath(normalized).drive:
        raise CatalogError("catalog contains an absolute folder path")
    parts = [part for part in normalized.split("/") if part not in ("", ".")]
    if any(part == ".." for part in parts):
        raise CatalogError("catalog folder path escapes its root")
    return parts


def catalog_uri(path: Path) -> str:
    return path.resolve().as_uri() + "?mode=ro&immutable=1"


def records(db: sqlite3.Connection):
    required = {
        "Adobe_AdditionalMetadata": {"image", "xmp"},
        "Adobe_images": {"id_local", "rootFile"},
        "AgLibraryFile": {"id_local", "baseName", "extension", "folder"},
        "AgLibraryFolder": {"id_local", "pathFromRoot", "rootFolder"},
        "AgLibraryRootFolder": {"id_local", "absolutePath", "name"},
    }
    for table, expected in required.items():
        if not expected.issubset(columns(db, table)):
            raise CatalogError(f"catalog table {table} is missing required fields")

    image_cols = columns(db, "Adobe_images")
    file_cols = columns(db, "AgLibraryFile")
    master_filter = "AND (i.masterImage IS NULL OR i.masterImage=0)" if "masterImage" in image_cols else ""
    import_hash = "f.importHash" if "importHash" in file_cols else "NULL"
    original_filename = "f.originalFilename" if "originalFilename" in file_cols else "NULL"
    sql = f"""
        SELECT i.id_local, f.baseName, f.extension, fo.pathFromRoot,
               ro.absolutePath, ro.name, ro.id_local, {import_hash}, {original_filename}, x.xmp
        FROM Adobe_AdditionalMetadata AS x
        JOIN Adobe_images AS i ON i.id_local=x.image
        JOIN AgLibraryFile AS f ON f.id_local=i.rootFile
        LEFT JOIN AgLibraryFolder AS fo ON fo.id_local=f.folder
        LEFT JOIN AgLibraryRootFolder AS ro ON ro.id_local=fo.rootFolder
        WHERE x.xmp IS NOT NULL {master_filter}
    """
    yield from db.execute(sql)


def source_path(base: str, extension: str, folder: str, root: str) -> Path | None:
    if not base or "/" in base or "\\" in base or base in (".", "..") or not root:
        return None
    try:
        parts = path_parts(folder or "")
    except CatalogError:
        return None
    ext = extension.lstrip(".") if extension else ""
    filename = f"{base}.{ext}" if ext else base
    root_path = Path(root)
    result = root_path.joinpath(*parts, filename)
    try:
        if os.path.commonpath((str(root_path.resolve()), str(result.resolve()))) != str(root_path.resolve()):
            return None
    except (OSError, ValueError):
        return None
    return result


def mapped_root(root: str, mappings: list[tuple[str, str]]) -> str:
    """Replace a catalog root prefix while keeping its relative folder structure."""
    normalized = root.replace("\\", "/").rstrip("/")
    folded = normalized.casefold()
    for old, new in mappings:
        old_normalized = old.replace("\\", "/").rstrip("/")
        old_folded = old_normalized.casefold()
        if folded == old_folded:
            return new
        if folded.startswith(old_folded + "/"):
            suffix = normalized[len(old_normalized):].lstrip("/")
            return str(Path(new).joinpath(*suffix.split("/")))
    return root


def safe_root_name(name: str, root: str, root_id: int) -> str:
    label = name.strip() or Path(root).name or "catalog-root"
    label = re.sub(r"[^A-Za-z0-9._-]+", "_", label).strip("._") or "catalog-root"
    return f"{label}-{root_id}"


def safe_output_component(value: str) -> str:
    clean = re.sub(r'[<>:"/\\|?*\x00-\x1f]', "_", value).rstrip(" .")
    return clean if clean not in ("", ".", "..") else "_"


def indexed_file_size(import_hash: str | None) -> int | None:
    if not import_hash:
        return None
    tail = import_hash.rsplit(":", 1)[-1]
    return int(tail) if tail.isdecimal() else None


def search_keys(db: sqlite3.Connection) -> dict[str, set[int]]:
    image_cols = columns(db, "Adobe_images")
    file_cols = columns(db, "AgLibraryFile")
    master_filter = "AND (i.masterImage IS NULL OR i.masterImage=0)" if "masterImage" in image_cols else ""
    import_hash = "f.importHash" if "importHash" in file_cols else "NULL"
    original_filename = "f.originalFilename" if "originalFilename" in file_cols else "NULL"
    sql = f"""
        SELECT f.baseName, f.extension, {import_hash}, {original_filename}
        FROM Adobe_AdditionalMetadata AS x
        JOIN Adobe_images AS i ON i.id_local=x.image
        JOIN AgLibraryFile AS f ON f.id_local=i.rootFile
        WHERE x.xmp IS NOT NULL {master_filter}
    """
    wanted: dict[str, set[int]] = {}
    for base, extension, import_hash_value, original in db.execute(sql):
        size = indexed_file_size(import_hash_value)
        if size is None:
            continue
        catalog_name = f"{base}.{extension.lstrip('.')}" if extension else (base or "")
        for name in (original or "", catalog_name):
            if name:
                basename = name.replace("\\", "/").rsplit("/", 1)[-1]
                wanted.setdefault(basename.casefold(), set()).add(size)
    return wanted


def build_search_index(
    roots: list[Path], wanted: dict[str, set[int]]
) -> dict[tuple[str, int], list[Path]]:
    index: dict[tuple[str, int], list[Path]] = {}
    seen_roots: set[str] = set()
    for root in roots:
        root = root.expanduser().resolve()
        if not root.is_dir():
            raise CatalogError(f"search root is not a directory: {root}")
        root_key = os.path.normcase(str(root))
        if root_key in seen_roots:
            continue
        seen_roots.add(root_key)
        files_seen = 0
        indexed = 0
        for directory, dirnames, filenames in os.walk(root, followlinks=False):
            dirnames[:] = [name for name in dirnames if not (Path(directory) / name).is_symlink()]
            for name in filenames:
                files_seen += 1
                sizes = wanted.get(name.casefold())
                if not sizes:
                    if files_seen % 25000 == 0:
                        print(f"Scanned {files_seen} directory entries under {root}; indexed {indexed} candidates.", file=sys.stderr, flush=True)
                    continue
                path = Path(directory) / name
                try:
                    size = path.stat().st_size
                except OSError:
                    continue
                if size in sizes:
                    index.setdefault((name.casefold(), size), []).append(path)
                    indexed += 1
                if files_seen % 25000 == 0:
                    print(f"Scanned {files_seen} directory entries under {root}; indexed {indexed} candidates.", file=sys.stderr, flush=True)
        print(f"Scanned {files_seen} directory entries under {root}; indexed {indexed} candidates.", file=sys.stderr, flush=True)
    return index


def find_unique_file(
    index: dict[tuple[str, int], list[Path]],
    names: list[str],
    size: int | None,
) -> tuple[Path | None, bool]:
    if size is None:
        return None, False
    matches: dict[str, Path] = {}
    for name in names:
        if name:
            basename = name.replace("\\", "/").rsplit("/", 1)[-1]
            for path in index.get((basename.casefold(), size), []):
                matches.setdefault(os.path.normcase(str(path)), path)
    if len(matches) == 1:
        return next(iter(matches.values())), False
    return None, len(matches) > 1


def exclusive_write(path: Path, data: bytes) -> str:
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o666)
    except FileExistsError:
        try:
            return "unchanged" if path.read_bytes() == data else "conflict"
        except OSError:
            return "conflict"
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
    except OSError:
        try:
            path.unlink()
        except OSError:
            pass
        raise
    return "written"


def write_destination(path: Path, data: bytes) -> str:
    try:
        return exclusive_write(path, data)
    except PermissionError:
        return "unwritable"


def xmp_destinations(
    *,
    source: Path | None,
    root: str,
    root_name: str,
    root_id: int,
    folder: str,
    base: str,
    output_dir: Path | None,
    in_place: bool,
) -> tuple[Path | None, list[Path]]:
    stem_sidecar = (source.with_name(source.stem + ".xmp") if source else None)
    full_sidecar = (source.with_name(source.name + ".xmp") if source else None)
    existing_checks = [p for p in (stem_sidecar, full_sidecar) if p is not None]
    if in_place:
        return stem_sidecar, existing_checks

    assert output_dir is not None
    mirror = output_dir / safe_root_name(root_name, root, root_id)
    mirror = mirror.joinpath(*(safe_output_component(p) for p in path_parts(folder or "")))
    return mirror / f"{safe_output_component(base)}.xmp", []


def run(args: argparse.Namespace) -> int:
    catalog = args.catalog.expanduser().resolve()
    if not catalog.is_file():
        raise CatalogError(f"catalog does not exist: {catalog}")
    if args.output_dir is not None:
        output_dir = args.output_dir.expanduser().resolve()
        if args.write and output_dir.exists() and any(output_dir.iterdir()):
            raise CatalogError("output directory must be new or empty; refusing to overwrite its contents")
    else:
        output_dir = None

    wal = Path(str(catalog) + "-wal")
    if wal.exists() and wal.stat().st_size:
        print("Warning: catalog has a non-empty -wal file; immutable read-only mode ignores it.", file=sys.stderr)

    db = sqlite3.connect(catalog_uri(catalog), uri=True, timeout=5)
    db.execute("PRAGMA query_only=ON")
    summary = {"packets": 0, "with_crs": 0, "written": 0, "unchanged": 0, "conflict": 0, "unwritable": 0, "missing_photo": 0, "photos_found": 0, "relocated": 0, "ambiguous": 0, "bad_packet": 0, "duplicates": 0}
    seen_sources: set[str] = set()
    manifest_rows: list[tuple[str, str, str, str]] = []
    unwritable_paths: list[str] = []
    rows_seen = 0
    mappings = args.root_map
    search_index = build_search_index(args.search_under, search_keys(db)) if args.search_under else {}
    for image_id, base, extension, folder, root, root_name, root_id, import_hash, original_filename, packed in records(db):
        rows_seen += 1
        if rows_seen % 5000 == 0:
            print(
                f"Scanned {rows_seen} catalog records; {summary['with_crs']} contain Camera Raw settings; "
                f"{summary['photos_found']} matching photos found.",
                file=sys.stderr,
                flush=True,
            )
        if args.limit is not None and summary["packets"] >= args.limit:
            break
        try:
            packet = decode_packet(packed)
        except CatalogError:
            summary["bad_packet"] += 1
            continue
        if packet is None:
            continue
        summary["packets"] += 1
        if CRS_NS not in packet:
            continue
        summary["with_crs"] += 1
        resolved_root = mapped_root(root or "", mappings)
        source = source_path(base or "", extension or "", folder or "", resolved_root)
        key = os.path.normcase(str(source)) if source else ""
        source_exists = args.in_place and source is not None and source.is_file()
        if args.in_place and not source_exists and search_index:
            catalog_name = f"{base}.{extension.lstrip('.')}" if extension else (base or "")
            candidate, ambiguous = find_unique_file(
                search_index,
                [original_filename or "", catalog_name],
                indexed_file_size(import_hash),
            )
            if candidate is not None:
                source = candidate
                key = os.path.normcase(str(source))
                source_exists = True
                summary["relocated"] += 1
            elif ambiguous:
                summary["ambiguous"] += 1
        if source_exists:
            summary["photos_found"] += 1
        if args.in_place and not source_exists:
            summary["missing_photo"] += 1
            action = "missing-photo"
            dest = None
        elif args.in_place and key in seen_sources:
            summary["duplicates"] += 1
            action = "duplicate-source-path"
            dest = None
        else:
            if key:
                seen_sources.add(key)
            dest, existing_checks = xmp_destinations(
                source=source,
                root=resolved_root,
                root_name=root_name or "",
                root_id=root_id,
                folder=folder or "",
                base=base or "",
                output_dir=output_dir,
                in_place=args.in_place,
            )
            if args.in_place:
                already = [p for p in existing_checks if p.exists()]
                if already:
                    action = "unchanged" if any(p.is_file() and p.read_bytes() == packet for p in already) else "conflict"
                elif args.write:
                    action = write_destination(dest, packet)
                else:
                    action = "would-write"
            elif args.write:
                action = write_destination(dest, packet)
            else:
                action = "would-write"

        if action in summary:
            summary[action] += 1
            if action == "unwritable" and dest is not None and len(unwritable_paths) < 10:
                unwritable_paths.append(str(dest))
        elif action == "would-write":
            summary["written"] += 1
        elif action == "unchanged":
            summary["unchanged"] += 1
        elif action == "conflict":
            summary["conflict"] += 1
        manifest_rows.append((str(image_id), str(source or ""), str(dest or ""), action))

    db.close()
    if args.write and output_dir is not None:
        output_dir.mkdir(parents=True, exist_ok=True)
        manifest = output_dir / "manifest.csv"
        with manifest.open("x", newline="", encoding="utf-8") as stream:
            writer = csv.writer(stream)
            writer.writerow(("catalog_image_id", "source_photo", "xmp_sidecar", "result"))
            writer.writerows(manifest_rows)

    print(f"Catalog: {catalog}")
    print(f"XMP packets read: {summary['packets']}; packets with Camera Raw settings: {summary['with_crs']}")
    if summary["bad_packet"]:
        print(f"Unreadable XMP packets skipped: {summary['bad_packet']}")
    if args.in_place:
        print(f"Sidecars written: {summary['written']}; already present: {summary['unchanged']}; conflicts: {summary['conflict']}; unwritable: {summary['unwritable']}")
        print(f"Photos unavailable at catalog paths: {summary['missing_photo']}; duplicate catalog paths skipped: {summary['duplicates']}")
        for path in unwritable_paths:
            print(f"Unwritable sidecar: {path}")
        if args.search_under:
            print(f"Photos relocated by unique filename and size: {summary['relocated']}; ambiguous matches skipped: {summary['ambiguous']}")
    else:
        print(f"Sidecars {'written' if args.write else 'that would be written'}: {summary['written']}; conflicts: {summary['conflict']}")
        if args.write:
            print(f"Output tree and manifest: {output_dir}")
    if args.limit is not None:
        print(f"Limited to the first {args.limit} packets for this run.")
    if not args.write:
        print("Dry run only; add --write to create files.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("catalog", type=Path, help="a Lightroom .lrcat catalog file, not a preview folder")
    destinations = parser.add_mutually_exclusive_group(required=True)
    destinations.add_argument("--output-dir", type=Path, help="export sidecars to a separate mirrored folder tree")
    destinations.add_argument("--in-place", action="store_true", help="write missing sidecars beside reachable original photos")
    parser.add_argument("--write", action="store_true", help="perform writes (default is a read-only dry run)")
    parser.add_argument(
        "--root-map",
        action="append",
        default=[],
        metavar="OLD=NEW",
        help="map a catalog photo root to its current location; repeat for multiple roots",
    )
    parser.add_argument(
        "--search-under",
        action="append",
        type=Path,
        default=[],
        metavar="DIR",
        help="search this photo tree for missing files with a unique catalog filename and file size; repeat as needed",
    )
    parser.add_argument("--limit", type=int, help="process at most N XMP packets; intended for dry runs")
    args = parser.parse_args()
    parsed_mappings = []
    for mapping in args.root_map:
        old, separator, new = mapping.partition("=")
        if not separator or not old.strip() or not new.strip():
            parser.error("--root-map must be OLD=NEW with both paths filled in")
        parsed_mappings.append((old.strip(), new.strip()))
    args.root_map = parsed_mappings
    if args.limit is not None and args.limit < 1:
        parser.error("--limit must be positive")
    if args.limit is not None and args.write:
        parser.error("--limit and --write cannot be used together")
    try:
        return run(args)
    except (CatalogError, OSError, sqlite3.Error, zlib.error) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
