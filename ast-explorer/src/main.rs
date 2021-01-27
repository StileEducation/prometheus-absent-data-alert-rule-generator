use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let res = prometheus_parser::parse_expr(&args[1]);

    match res {
        Ok(r) => {
            println!("{:#?}", r);
        }
        Err(e) => {
            eprintln!("error: {}", e);
        }
    };
}
