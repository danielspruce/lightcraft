import argparse
import contextlib
import io
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import extract_lightroom_xmp as extractor


class RawPreferenceTests(unittest.TestCase):
    def test_relocated_copies_each_receive_raw_packet(self):
        for write in (False, True):
            with self.subTest(write=write), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                catalog = root / "catalog.db"
                sqlite3.connect(catalog).close()
                copies = [root / folder / "photo.CR2" for folder in ("one", "two", "three")]
                for photo in copies:
                    photo.parent.mkdir()
                    photo.write_bytes(b"raw")
                existing = copies[2].with_suffix(".xmp")
                existing.write_bytes(b"existing")
                packet = b"<raw " + extractor.CRS_NS + b"/>"
                jpeg = b"<jpeg " + extractor.CRS_NS + b"/>"
                rows = [(1, "photo", "CR2", "", str(root / "missing"), "photos", 1, "hash:3", None, packet),
                        (2, "photo", "JPG", "", str(root / "missing"), "photos", 1, "hash:3", None, jpeg)]
                index = {("photo.cr2", 3): copies + [copies[0]],
                         ("photo.jpg", 3): [p.with_suffix(".JPG") for p in copies]}
                args = argparse.Namespace(catalog=catalog, output_dir=None, in_place=True,
                                          write=write, root_map=[], search_under=[root], limit=None)
                log = io.StringIO()
                with patch.object(extractor, "records", return_value=rows), \
                     patch.object(extractor, "search_keys", return_value={}), \
                     patch.object(extractor, "build_search_index", return_value=index), \
                     contextlib.redirect_stdout(log):
                    extractor.run(args)
                for photo in copies[:2]:
                    sidecar = photo.with_suffix(".xmp")
                    if write:
                        self.assertEqual(sidecar.read_bytes(), packet)
                    else:
                        self.assertFalse(sidecar.exists())
                self.assertEqual(existing.read_bytes(), b"existing")
                self.assertIn("Sidecars written: 2;", log.getvalue())
                self.assertIn("conflicts: 1;", log.getvalue())
                self.assertIn("paths skipped: 3", log.getvalue())

    def test_shared_sidecar_prefers_raw_in_both_modes(self):
        for in_place in (True, False):
            for write in (True, False):
                with self.subTest(in_place=in_place, write=write), tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    catalog = root / "catalog.db"
                    raw_packet = b"<raw " + extractor.CRS_NS + b"/>"
                    jpeg_packet = b"<jpeg " + extractor.CRS_NS + b"/>"
                    with sqlite3.connect(catalog) as db:
                        db.executescript("""
                            CREATE TABLE Adobe_AdditionalMetadata (image INTEGER, xmp BLOB);
                            CREATE TABLE Adobe_images (id_local INTEGER, rootFile INTEGER);
                            CREATE TABLE AgLibraryFile (id_local INTEGER, baseName TEXT, extension TEXT, folder INTEGER);
                            CREATE TABLE AgLibraryFolder (id_local INTEGER, pathFromRoot TEXT, rootFolder INTEGER);
                            CREATE TABLE AgLibraryRootFolder (id_local INTEGER, absolutePath TEXT, name TEXT);
                            INSERT INTO Adobe_images VALUES (1, 1), (2, 2), (3, 3);
                            INSERT INTO AgLibraryFile VALUES (1, 'photo', 'JPG', 1), (2, 'photo', '.Cr2', 1), (3, 'photo', 'JPG', 2);
                            INSERT INTO AgLibraryFolder VALUES (1, '', 1), (2, 'other', 1);
                        """)
                        db.execute("INSERT INTO AgLibraryRootFolder VALUES (1, ?, 'photos')", (str(root),))
                        db.executemany("INSERT INTO Adobe_AdditionalMetadata VALUES (?, ?)",
                                       [(1, jpeg_packet), (2, raw_packet), (3, jpeg_packet)])
                    (root / "photo.JPG").touch()
                    (root / "photo.Cr2").touch()
                    (root / "other").mkdir()
                    (root / "other/photo.JPG").touch()
                    output = None if in_place else root / "export"
                    db.close()
                    args = argparse.Namespace(catalog=catalog, output_dir=output, in_place=in_place,
                                              write=write, root_map=[], search_under=[], limit=None)
                    log = io.StringIO()
                    with contextlib.redirect_stdout(log):
                        self.assertEqual(extractor.run(args), 0)
                    self.assertIn("paths skipped: 1" if in_place else "duplicates skipped: 1", log.getvalue())
                    if write:
                        destination = root if in_place else output / "photos-1"
                        self.assertEqual((destination / "photo.xmp").read_bytes(), raw_packet)
                        self.assertEqual((destination / "other/photo.xmp").read_bytes(), jpeg_packet)
                    else:
                        self.assertFalse(list(root.rglob("*.xmp")))
                        self.assertIn("2;", log.getvalue())
                    if in_place and write:
                        (root / "photo.xmp").write_bytes(b"existing sidecar")
                        with contextlib.redirect_stdout(io.StringIO()):
                            extractor.run(args)
                        self.assertEqual((root / "photo.xmp").read_bytes(), b"existing sidecar")


if __name__ == "__main__":
    unittest.main()
