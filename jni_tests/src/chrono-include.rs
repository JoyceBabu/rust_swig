mod swig_foreign_types_map {}

foreign_typemap!(
    ($p:r_type) DateTime<Utc> => jlong {
        $out = $p.timestamp_millis()
    };
    ($p:f_type, option = "NoNullAnnotations", unique_prefix = "/*chrono*/") => "/*chrono*/java.util.Date" "$out = new java.util.Date($p);";
    ($p:f_type, option = "NullAnnotations", unique_prefix = "/*chrono*/") => "/*chrono*/@NonNull java.util.Date" "$out = new java.util.Date($p);";
);
