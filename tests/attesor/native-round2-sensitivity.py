#!/usr/bin/env python3
"""Mutate only a private shim: vadd -> vmul must fail full numeric comparison."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
spec=importlib.util.spec_from_file_location("check",Path(__file__).with_name("native-round2-check.py"))
check=importlib.util.module_from_spec(spec);spec.loader.exec_module(check)
base=Path("target/native-round2-check").resolve()
assert (base/"arm64").is_file(),"run native-round2-check.py first"
with tempfile.TemporaryDirectory(prefix="attesor-r2-sensitivity-",dir="/private/tmp") as directory:
    root=Path(directory);source=root/"vdsp-shim.c"
    original=Path("native/vdsp-shim.c").read_text()
    assert original.count("BINARY(vDSP_vadd,37)")==1
    source.write_text(original.replace("BINARY(vDSP_vadd,37)","BINARY(vDSP_vadd,38)"))
    shim=root/"libattesorvdsp.dylib"
    env={k:v for k,v in os.environ.items() if not k.startswith("ATTESOR_") and k!="DYLD_INSERT_LIBRARIES"}
    env.update(TMPDIR="/private/tmp",ATTESOR_CACHE=str(base/"cache"))
    subprocess.run(["clang","-arch","arm64","-O2","-dynamiclib","-I",str(Path("native").resolve()),"-framework","Accelerate",str(source),"-o",str(shim)],env=env,check=True)
    subprocess.run(["codesign","-s","-","-f",str(shim)],check=True,env=env)
    result=root/"mutant.bin"
    with result.open("wb") as out:
        r=subprocess.run([str(Path("target/release/attesor").resolve()),"--no-aot",str(base/"arm64"),"numeric","unused","1"],env={**env,"ATTESOR_NATIVE_LIBS":"vdsp","ATTESOR_NATIVE_DIR":str(root)},stdout=out,stderr=subprocess.PIPE,check=True,timeout=180)
    assert b"attesor_native_vdsp_calls=269" in r.stderr,r.stderr
    try:check.compare(base/"numeric-off-1.bin",result)
    except AssertionError as error:print(f"PASS sensitivity: private vadd -> vmul rejected: {error}")
    else:raise AssertionError("numeric comparison accepted deliberately wrong host arithmetic")
