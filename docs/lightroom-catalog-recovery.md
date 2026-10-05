# Recovering Lightroom edits without Lightroom

`tools/extract_lightroom_xmp.py` copies standard XMP packets from a Lightroom `.lrcat` catalog. It uses Python's
standard library and opens the catalog read-only. It supports the plain XML packets used by older catalogs and the
length-prefixed zlib packets used by newer catalogs.

The catalog records this library's photo roots as `G:\Users\spruc\Pictures` and
`C:\Documents and Settings\Ladies\My Documents\My Pictures`, while the current folder is `D:\Users\spruc\Pictures`.
Start with a dry run against the newest `.lrcat` file (ignore `Previews.lrdata` and `Helper.lrdata` folders):

```powershell
python tools\extract_lightroom_xmp.py "D:\Users\spruc\Pictures\Lightroom\Lightroom Catalog-v11.lrcat" --in-place --root-map "G:\Users\spruc\Pictures=D:\Users\spruc\Pictures" --root-map "C:\Documents and Settings\Ladies\My Documents\My Pictures=D:\Users\spruc\Pictures" --search-under "D:\Users\spruc\Pictures"
```

The dry run reports how many packets contain Camera Raw settings, how many source photos are reachable at the paths
recorded in the catalog, and whether matching XMP files already exist. To write missing sidecars next to reachable
photos, add `--write` to the same command:

```powershell
python tools\extract_lightroom_xmp.py "D:\Users\spruc\Pictures\Lightroom\Lightroom Catalog-v11.lrcat" --in-place --root-map "G:\Users\spruc\Pictures=D:\Users\spruc\Pictures" --root-map "C:\Documents and Settings\Ladies\My Documents\My Pictures=D:\Users\spruc\Pictures" --search-under "D:\Users\spruc\Pictures" --write
```

The `--root-map OLD=NEW` option replaces a saved catalog root while preserving its relative folders; repeat it for
other moved roots. The script never replaces an existing sidecar. It reports conflicts when a different XMP file
already exists. If the original photos are on unavailable drives, export the packets to a separate mirrored folder
tree instead:

```powershell
python tools\extract_lightroom_xmp.py "D:\Users\spruc\Pictures\Lightroom\Lightroom Catalog-v11.lrcat" --output-dir "D:\Users\spruc\Pictures\Recovered-XMP" --write
```

That export includes a `manifest.csv`; its sidecars must be placed beside their matching photos before LightCraft will
find them automatically.

When raw and rendered photos in the same folder share a filename stem (for example, `IMG_0001.CR2` and
`IMG_0001.JPG`), the extractor prefers the raw photo's XMP packet for `IMG_0001.xmp`. Later records targeting
that same sidecar are counted as duplicates and skipped in both dry runs and writes, in either destination mode.
Existing sidecars are still never overwritten. Identical stems in different folders remain separate.

The script extracts the current XMP packet per original photo. It does not convert Lightroom's edit history, collections,
or proprietary catalog records. LightCraft maps common `crs:` develop fields approximately; unmapped settings such as
camera profiles, local masks, and spot repairs remain unavailable. If the catalog has a non-empty `-wal` file, immutable
read-only mode ignores it, so use a closed, consistent catalog backup for the most complete result.

If photos were reorganized below the mapped roots, `--search-under DIR` enables a second pass over a photo tree. It
matches missing records by the catalog's filename and imported file size. When several copies match, it writes the
same packet beside every matching copy, preserving any existing sidecars. The summary counts records with multiple
matching copies separately. Preview this before writing; the search builds a filename/size index and reports scan progress.
