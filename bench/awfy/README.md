# Are-We-Fast-Yet benchmarks

The JavaScript benchmarks of [Are We Fast Yet?](https://github.com/smarr/are-we-fast-yet)
(Marr, Daloze, Mössenböck, DLS 2016), unmodified, from upstream commit
`f35aed0a61171594804a44efe9d75f1f35360700`. Licenses: see `LICENSE.md`.

They are rts's Node-compatibility finish line: each must run unmodified with
output identical to Node's.

    bench/awfy/compare.sh              # test sizes (fast)
    SIZE=full bench/awfy/compare.sh    # the suite's standard sizes
