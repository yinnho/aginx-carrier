fn main() {
    uniffi::generate_scaffolding("src/carrier.udl").expect("generate scaffolding");
}
