use criterion::criterion_main;

mod bench_hashmap;
mod bench_list;
mod bench_vec;


fn bencher() {
    println!("====== Benchmark Suite ======");

    bench_hashmap::run();
    bench_list::run();
    bench_vec::run();

    println!("======================");
}

criterion_main!(bencher);