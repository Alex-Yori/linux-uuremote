//! 无依赖的 protobuf wire 检查，用于官方二进制控制通道（be-controlled 的 app_data 等）。
//!
//! 只解码字段，不赋予心跳/输入语义，也不发 ack——那些要等应用层回调证据。
//! 诊断渲染只输出结构（字段号、类型、长度、小 varint），绝不输出内容：
//! 载荷里可能是 token、设备 id 这类机密。

#[derive(Debug, PartialEq, Eq)]
pub enum WireValue<'a> {
    Varint(u64),
    Fixed64([u8; 8]),
    Bytes(&'a [u8]),
    Fixed32([u8; 4]),
}

#[derive(Debug, PartialEq, Eq)]
pub struct WireField<'a> {
    pub number: u32,
    pub value: WireValue<'a>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    VarintOverflow,
    InvalidField,
    UnsupportedWireType(u8),
}

fn varint(input: &[u8], offset: &mut usize) -> Result<u64, WireError> {
    let mut result = 0;
    for shift in (0..70).step_by(7) {
        let byte = *input.get(*offset).ok_or(WireError::Truncated)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err(WireError::VarintOverflow);
        }
        result |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(result);
        }
    }
    Err(WireError::VarintOverflow)
}

fn take<'a>(input: &'a [u8], offset: &mut usize, length: usize) -> Result<&'a [u8], WireError> {
    let end = offset.checked_add(length).ok_or(WireError::Truncated)?;
    let value = input.get(*offset..end).ok_or(WireError::Truncated)?;
    *offset = end;
    Ok(value)
}

/// 解析完整 protobuf 消息，嵌套载荷保持不透明。deprecated group 直接拒绝。
pub fn decode_wire(input: &[u8]) -> Result<Vec<WireField<'_>>, WireError> {
    let mut offset = 0;
    let mut fields = Vec::new();
    while offset < input.len() {
        let key = varint(input, &mut offset)?;
        let number = key >> 3;
        if number == 0 || number > 0x1fff_ffff {
            return Err(WireError::InvalidField);
        }
        let value = match (key & 7) as u8 {
            0 => WireValue::Varint(varint(input, &mut offset)?),
            1 => WireValue::Fixed64(take(input, &mut offset, 8)?.try_into().unwrap()),
            2 => {
                let length = usize::try_from(varint(input, &mut offset)?).map_err(|_| WireError::Truncated)?;
                WireValue::Bytes(take(input, &mut offset, length)?)
            }
            5 => WireValue::Fixed32(take(input, &mut offset, 4)?.try_into().unwrap()),
            wire => return Err(WireError::UnsupportedWireType(wire)),
        };
        fields.push(WireField { number: number as u32, value });
    }
    Ok(fields)
}

/// 取某 varint 字段（取第一个），用于 capture_type 这类小枚举。
pub fn varint_field(fields: &[WireField<'_>], number: u32) -> Option<u64> {
    fields.iter().find_map(|f| match &f.value {
        WireValue::Varint(v) if f.number == number => Some(*v),
        _ => None,
    })
}

/// 取某嵌套 bytes 字段并再解析一层（如 ConnectOptions 的 decoder_cap_list）。
pub fn nested<'a>(fields: &[WireField<'a>], number: u32) -> Option<Vec<WireField<'a>>> {
    fields.iter().find_map(|f| match &f.value {
        WireValue::Bytes(b) if f.number == number => decode_wire(b).ok(),
        _ => None,
    })
}

/// 结构渲染：字段号、wire 类型、长度、小 varint。字符串与字节内容永不输出。
pub fn wire_shape(data: &[u8], depth: u8) -> String {
    let Ok(fields) = decode_wire(data) else {
        return format!("bytes({})", data.len());
    };
    let parts: Vec<String> = fields
        .iter()
        .map(|f| match f.value {
            WireValue::Varint(u64::MAX) => format!("{}=-1", f.number),
            WireValue::Varint(v) if v <= 0xFFFF => format!("{}={v}", f.number),
            WireValue::Varint(_) => format!("{}=big", f.number),
            WireValue::Fixed32(_) => format!("{}:f32", f.number),
            WireValue::Fixed64(_) => format!("{}:f64", f.number),
            WireValue::Bytes(b) => format!("{}:{}", f.number, bytes_shape(b, depth)),
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn bytes_shape(b: &[u8], depth: u8) -> String {
    let printable = !b.is_empty() && std::str::from_utf8(b).is_ok_and(|t| t.chars().all(|c| !c.is_control()));
    if printable {
        return format!("str({})", b.len());
    }
    if depth > 0
        && !b.is_empty()
        && decode_wire(b).is_ok_and(|f| !f.is_empty() && f.iter().all(|x| (1..=128).contains(&x.number)))
    {
        return wire_shape(b, depth - 1);
    }
    format!("bytes({})", b.len())
}

/// JSON 侧的结构渲染：键与值类型；字符串只给长度（或嵌套 JSON / base64-protobuf 的形状），
/// 小数才给数值。可能识别出用户身份的值永不输出。
pub fn json_shape(value: &serde_json::Value, depth: u8) -> String {
    use base64::Engine;
    use serde_json::Value;
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(v) if v.abs() <= 0xFFFF => v.to_string(),
            _ => "num".into(),
        },
        Value::String(t) => {
            if depth > 0 {
                if let Ok(inner @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str::<Value>(t) {
                    return format!("json{}", json_shape(&inner, depth - 1));
                }
                if t.len() >= 8 && t.len() % 4 == 0 {
                    if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(t) {
                        if decode_wire(&raw).is_ok_and(|f| !f.is_empty()) {
                            return format!("b64{}", wire_shape(&raw, depth - 1));
                        }
                    }
                }
            }
            format!("str({})", t.len())
        }
        Value::Array(items) => match items.first() {
            Some(first) if depth > 0 => format!("[{}x{}]", items.len(), json_shape(first, depth - 1)),
            _ => format!("[{}]", items.len()),
        },
        Value::Object(map) if depth > 0 => {
            let parts: Vec<String> = map
                .iter()
                .take(48)
                .map(|(k, v)| {
                    let key: String = k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect();
                    format!("{key}:{}", json_shape(v, depth - 1))
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Object(map) => format!("{{{} keys}}", map.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_mixed_unknown_fields_and_nested_bytes() {
        let bytes = [8, 150, 1, 18, 2, 8, 3, 29, 1, 2, 3, 4];
        assert_eq!(
            decode_wire(&bytes),
            Ok(vec![
                WireField { number: 1, value: WireValue::Varint(150) },
                WireField { number: 2, value: WireValue::Bytes(&[8, 3]) },
                WireField { number: 3, value: WireValue::Fixed32([1, 2, 3, 4]) },
            ])
        );
    }

    #[test]
    fn rejects_truncation_and_u64_overflow() {
        assert_eq!(decode_wire(&[18, 4, 1]), Err(WireError::Truncated));
        assert_eq!(decode_wire(&[8, 128]), Err(WireError::Truncated));
        let mut bytes = vec![8];
        bytes.extend_from_slice(&[255; 9]);
        bytes.push(2);
        assert_eq!(decode_wire(&bytes), Err(WireError::VarintOverflow));
    }

    #[test]
    fn rejects_invalid_fields_and_groups() {
        assert_eq!(decode_wire(&[0]), Err(WireError::InvalidField));
        assert_eq!(decode_wire(&[11]), Err(WireError::UnsupportedWireType(3)));
        assert_eq!(decode_wire(&[255, 255, 255, 255, 31]), Err(WireError::InvalidField));
    }

    #[test]
    fn shapes_never_render_contents() {
        // {1:1, 2:-1, 4:{1:60,2:2}, 9:"device-secret", 12:big}
        let mut data = vec![0x08, 0x01, 0x10];
        data.extend([0xff; 9]);
        data.push(0x01);
        data.extend([0x22, 0x04, 0x08, 0x3c, 0x10, 0x02]);
        data.extend([0x4a, 0x0d]);
        data.extend(b"device-secret");
        data.extend([0x60, 0x80, 0x80, 0x80, 0x80, 0x10]);
        let shape = wire_shape(&data, 3);
        assert_eq!(shape, "{1=1,2=-1,4:{1=60,2=2},9:str(13),12=big}");
        let header = serde_json::json!({"streamer_data": "{\"token\":\"secret-value\",\"n\":3}", "city": "Somewhere", "id": 123456789});
        let shape = json_shape(&header, 4);
        assert!(!shape.contains("secret") && !shape.contains("Somewhere") && !shape.contains("123456789"), "{shape}");
        assert!(shape.contains("streamer_data:json{n:3,token:str(12)}"), "{shape}");
    }

    #[test]
    fn field_helpers_read_enums_and_nested_lists() {
        // ConnectOptions 形状：{1: capture_type, 4: [DecoderCap{1:fps,2:codec}]}
        let mut data = vec![0x08, 0x02];
        data.extend([0x22, 0x04, 0x08, 0x3c, 0x10, 0x02]);
        let fields = decode_wire(&data).unwrap();
        assert_eq!(varint_field(&fields, 1), Some(2));
        let caps: Vec<Vec<(u32, u64)>> = fields
            .iter()
            .filter(|f| f.number == 4)
            .filter_map(|f| match &f.value {
                WireValue::Bytes(b) => decode_wire(b).ok().map(|cap| {
                    cap.iter()
                        .filter_map(|x| match x.value {
                            WireValue::Varint(v) => Some((x.number, v)),
                            _ => None,
                        })
                        .collect()
                }),
                _ => None,
            })
            .collect();
        assert!(caps.iter().any(|c| c.contains(&(1, 60)) && c.contains(&(2, 2))), "{caps:?}");
        assert!(nested(&fields, 4).is_some());
        assert_eq!(varint_field(&fields, 9), None);
    }
}
