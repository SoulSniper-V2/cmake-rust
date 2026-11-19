use super::Interpreter;
use crate::{Result, fail};

impl Interpreter {
    pub(super) fn math(&mut self, args: &[String]) -> Result<()> {
        let (output, expression, hexadecimal) = match args {
            [operation, output, expression] if operation == "EXPR" => (output, expression, false),
            [operation, output, expression, option, format]
                if operation == "EXPR"
                    && option == "OUTPUT_FORMAT"
                    && matches!(format.as_str(), "DECIMAL" | "HEXADECIMAL") =>
            {
                (output, expression, format == "HEXADECIMAL")
            }
            _ => return fail(format!("unsupported math arguments {args:?}")),
        };
        let mut parser = Expression(expression);
        let number = parser.evaluate(0)?;
        if !parser.rest().is_empty() {
            return fail(format!("unexpected math input {:?}", parser.rest()));
        }
        let value = if hexadecimal {
            format!("0x{number:x}")
        } else {
            number.to_string()
        };
        self.set(output, value);
        Ok(())
    }
}

struct Expression<'a>(&'a str);

impl<'a> Expression<'a> {
    fn rest(&self) -> &'a str {
        self.0.trim_start()
    }

    fn consume(&mut self, token: &str) -> bool {
        if let Some(tail) = self.rest().strip_prefix(token) {
            self.0 = tail;
            true
        } else {
            false
        }
    }

    fn evaluate(&mut self, minimum: u8) -> Result<i64> {
        let mut value = self.operand()?;
        const OPERATORS: &[(&str, u8)] = &[
            ("<<", 4),
            (">>", 4),
            ("|", 1),
            ("^", 2),
            ("&", 3),
            ("+", 5),
            ("-", 5),
            ("*", 6),
            ("/", 6),
            ("%", 6),
        ];
        while let Some(&(token, precedence)) = OPERATORS
            .iter()
            .find(|(token, _)| self.rest().starts_with(token))
        {
            if precedence < minimum {
                break;
            }
            self.consume(token);
            let right = self.evaluate(precedence + 1)?;
            value = match token {
                "|" => value | right,
                "^" => value ^ right,
                "&" => value & right,
                "<<" => value.wrapping_shl(shift(right)?),
                ">>" => value.wrapping_shr(shift(right)?),
                "+" => value.wrapping_add(right),
                "-" => value.wrapping_sub(right),
                "*" => value.wrapping_mul(right),
                "/" | "%" if right == 0 => return fail("division by zero"),
                "/" => value.wrapping_div(right),
                "%" => value.wrapping_rem(right),
                _ => unreachable!("operators are exhaustive"),
            };
        }
        Ok(value)
    }

    fn operand(&mut self) -> Result<i64> {
        if self.consume("(") {
            let result = self.evaluate(0)?;
            if !self.consume(")") {
                return fail("math expression is missing ')'");
            }
            return Ok(result);
        }
        if self.consume("+") {
            return self.operand();
        }
        if self.consume("-") {
            return Ok(self.operand()?.wrapping_neg());
        }
        if self.consume("~") {
            return Ok(!self.operand()?);
        }
        let rest = self.rest();
        let (digits, radix) =
            if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
                (hex, 16)
            } else {
                (rest, 10)
            };
        let len = digits.chars().take_while(|ch| ch.is_digit(radix)).count();
        if len == 0 {
            return fail(format!("expected math operand at {rest:?}"));
        }
        self.0 = &digits[len..];
        if radix == 16 {
            Ok(u64::from_str_radix(&digits[..len], radix)? as i64)
        } else {
            Ok(digits[..len].parse()?)
        }
    }
}

fn shift(number: i64) -> Result<u32> {
    if !(0..64).contains(&number) {
        return fail(format!("shift count out of range: {number}"));
    }
    Ok(number as u32)
}
