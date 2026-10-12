use serde_json::{Value, json};

pub fn cases() -> Vec<Value> {
    let manifest: Value = serde_json::from_slice(include_bytes!(
        "../../../../tests/golden/docker-image-checks/decode/cases.json"
    ))
    .unwrap();
    assert_eq!(manifest["schema_version"], 1);
    manifest["cases"].as_array().unwrap().clone()
}

fn field<'a>(document: &'a mut Value, path: &Value) -> &'a mut Value {
    let mut field = document;
    for key in path.as_array().unwrap() {
        field = &mut field[key.as_str().unwrap()];
    }
    field
}

pub fn payload(case: &Value) -> Vec<u8> {
    let document: Value = serde_json::from_slice(include_bytes!(
        "../../../../tests/golden/docker-image-checks/fixtures/image.json"
    ))
    .unwrap();
    response(document, case)
}

pub fn response(mut document: Value, case: &Value) -> Vec<u8> {
    if let Some(raw) = case["hex"].as_str() {
        return raw
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
    }
    if let Some(changes) = case["changes"].as_array() {
        for change in changes {
            *field(&mut document, &change["path"]) = change["value"].clone();
        }
    }
    if let Some(paths) = case["remove"].as_array() {
        for path in paths {
            let mut parent = &mut document;
            let keys = path.as_array().unwrap();
            for key in &keys[..keys.len() - 1] {
                parent = &mut parent[key.as_str().unwrap()];
            }
            parent
                .as_object_mut()
                .unwrap()
                .remove(keys.last().unwrap().as_str().unwrap());
        }
    }
    if case["repeat"].is_object() {
        let repeat = &case["repeat"];
        let count = repeat["count"].as_u64().unwrap() as usize;
        *field(&mut document, &repeat["path"]) = if repeat["array"] == true {
            json!(vec![repeat["value"].clone(); count])
        } else {
            json!(
                repeat["value"].as_str().unwrap().repeat(count)
                    + repeat["tail"].as_str().unwrap_or("")
            )
        };
    }
    if let Some(depth) = case["depth"].as_u64() {
        let mut nested = json!(0);
        for _ in 2..depth {
            nested = json!([nested]);
        }
        document["unknown"] = nested;
    }
    if let Some(size) = case["padding_bytes"].as_u64() {
        document["padding"] = json!("");
        let baseline = serde_json::to_vec(&document).unwrap().len();
        document["padding"] = json!("a".repeat(size as usize - baseline));
    }
    serde_json::to_vec(&document).unwrap()
}
