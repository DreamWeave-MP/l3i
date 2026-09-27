//! `vector:writef32x3` (nativevectorbuffer.cpp, interpreter path).

use dream_binder::Runtime;

#[test]
fn writef32x3_matches_three_writef32_calls_and_keeps_other_methods_intact() {
    let runtime = Runtime::new().unwrap();
    runtime.install_vector_buffer_writer().unwrap();
    let probe = runtime
        .load_function(
            "return function()\n\
               local a, b = buffer.create(16), buffer.create(16)\n\
               local v = vector.create(1.5, -2.25, 1e10)\n\
               v:writef32x3(a, 4)\n\
               buffer.writef32(b, 4, 1.5) buffer.writef32(b, 8, -2.25) buffer.writef32(b, 12, 1e10)\n\
               assert(buffer.tostring(a) == buffer.tostring(b), 'same bytes')\n\
               assert(buffer.readf32(a, 4) == 1.5 and buffer.readf32(a, 12) == 1e10)\n\
               local ok, err = pcall(function() v:writef32x3(a, 5) end)\n\
               assert(not ok and err:find('buffer access out of bounds'), err)\n\
               ok, err = pcall(function() v:writef32x3(a, -1) end)\n\
               assert(not ok and err:find('buffer access out of bounds'), err)\n\
               ok, err = pcall(function() v:writef32x3('nope', 0) end)\n\
               assert(not ok and err:find('buffer expected'), err)\n\
               ok, err = pcall(function() return v:nosuchmethod() end)\n\
               assert(not ok and err:find('attempt to index vector with'), err)\n\
               return v.x + v.y\n\
             end",
        )
        .unwrap();
    assert_eq!(probe.invoke::<f64, _>(&runtime.stack(), ()).unwrap(), -0.75);
    let error = runtime.install_vector_buffer_writer().unwrap_err().to_string();
    assert!(error.contains("already has a __namecall"), "{error}");
}
