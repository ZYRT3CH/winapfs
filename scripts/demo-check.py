"""Checks the winapfs demo mount through ordinary Windows file APIs."""
import os
import random
import shutil
import sys

root = sys.argv[1]
fails = []


def check(cond, msg):
    print(("ok    " if cond else "FAIL  ") + msg)
    if not cond:
        fails.append(msg)


def demo_byte(i):
    return ((i * 31) ^ (i >> 9)) & 0xFF


names = sorted(os.listdir(root))
odd = "Folder with colon"
check(names == sorted([".hidden", "hello.txt", odd, "Råfiler", "many"]), f"root listing {names}")
check(open(os.path.join(root, "hello.txt"), "rb").read() == b"Hello from winapfs\r\n", "hello.txt content")
nordic = os.path.join(root, "Råfiler", "æøå.txt")
check(open(nordic, "rb").read() == "blåbærsyltetøy\r\n".encode(), "Norwegian names and UTF-8 content")
check(open(os.path.join(root, "HELLO.TXT"), "rb").read().startswith(b"Hello"), "case-insensitive open")

inner = sorted(os.listdir(os.path.join(root, odd)))
check(inner == sorted(["big.bin", "notes quoted.txt", "trailing space"]), f"mapped names {inner}")
check(open(os.path.join(root, odd, "notes quoted.txt"), "rb").read() == b"quoted\r\n", "open a name with mapped quotes")
check(open(os.path.join(root, odd, "trailing space"), "rb").read() == b"space\r\n", "open a name with a mapped trailing space")

big = os.path.join(root, odd, "big.bin")
n = 10 * 1024 * 1024 + 123
check(os.path.getsize(big) == n, "big.bin size")
data = open(big, "rb").read()
check(len(data) == n and all(data[i] == demo_byte(i) for i in range(0, n, 4099)), "big.bin sequential read")
rng = random.Random(7)
with open(big, "rb") as f:
    ok = True
    for _ in range(300):
        off = rng.randrange(0, n)
        ln = rng.randrange(1, 300_000)
        f.seek(off)
        got = f.read(ln)
        want = data[off:off + ln]
        ok &= got == want and len(want) == min(ln, n - off)
    check(ok, "big.bin 300 random-offset reads")

many = os.listdir(os.path.join(root, "many"))
check(len(many) == 2000 and len(set(many)) == 2000, f"2000-entry folder lists fully ({len(many)} entries)")
check(sorted(many) == [f"file {i:04}.txt" for i in range(2000)], "2000-entry folder: exact names")
check(open(os.path.join(root, "many", "file 1234.txt"), "rb").read() == b"1234\r\n", "file in big folder")

st = os.stat(os.path.join(root, "hello.txt"))
check(int(st.st_mtime) == 1_714_521_600, f"modification time {st.st_mtime}")
check(bool(os.stat(os.path.join(root, ".hidden")).st_file_attributes & 2), ".hidden has the hidden attribute")
total, used, free = shutil.disk_usage(root)
check(total == 1 << 30 and free == 0, f"volume size total={total} free={free}")

for what, op in [
    ("create a file", lambda: open(os.path.join(root, "new.txt"), "wb")),
    ("overwrite a file", lambda: open(os.path.join(root, "hello.txt"), "wb")),
    ("delete a file", lambda: os.remove(os.path.join(root, "hello.txt"))),
    ("rename a file", lambda: os.rename(os.path.join(root, "hello.txt"), os.path.join(root, "x.txt"))),
    ("make a folder", lambda: os.mkdir(os.path.join(root, "newdir"))),
]:
    try:
        op()
        check(False, f"{what} was refused")
    except OSError as e:
        check(True, f"{what} was refused ({e.strerror})")

copy_dir = os.path.join(os.path.dirname(root), "demo-copy")
shutil.rmtree(copy_dir, ignore_errors=True)
shutil.copytree(os.path.join(root, odd), copy_dir)
check(open(os.path.join(copy_dir, "big.bin"), "rb").read() == data, "copy a folder out and compare")
shutil.rmtree(copy_dir, ignore_errors=True)

print(f"\n{len(fails)} failure(s)")
sys.exit(1 if fails else 0)
