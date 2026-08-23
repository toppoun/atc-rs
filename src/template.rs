use crate::language::Language;

const CPP_TEMPLATE: &str = include_str!("../assets/templates/default.cpp");

const PYTHON_TEMPLATE: &str = include_str!("../assets/templates/default.py");

const STRESS_GENERATOR_TEMPLATE: &str = include_str!("../assets/templates/stress_gen.py");

const STRESS_BRUTE_TEMPLATE: &str = include_str!("../assets/templates/stress_brute.py");

pub fn builtin_template(language: Language) -> &'static str {
    match language {
        Language::Cpp => CPP_TEMPLATE,
        Language::Python => PYTHON_TEMPLATE,
    }
}

pub(crate) fn stress_generator_template() -> &'static str {
    STRESS_GENERATOR_TEMPLATE
}

pub(crate) fn stress_brute_template() -> &'static str {
    STRESS_BRUTE_TEMPLATE
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_STRESS_GENERATOR: &str = concat!(
        "import random\n",
        "import string\n",
        "import sys\n",
        "\n",
        "\n",
        "def ni(lo: int, hi: int) -> int:\n",
        "    return random.randint(lo, hi)\n",
        "\n",
        "\n",
        "def nl(amount: int, lo: int, hi: int) -> str:\n",
        "    return \" \".join(str(ni(lo, hi)) for _ in range(amount))\n",
        "\n",
        "\n",
        "def si(length: int = 1, chars: str = string.ascii_lowercase) -> str:\n",
        "    return \"\".join(random.choice(chars) for _ in range(length))\n",
        "\n",
        "\n",
        "def main() -> None:\n",
        "    seed = int(sys.argv[1])\n",
        "    random.seed(seed)\n",
        "\n",
        "    # TODO: generate one test case\n",
        "    n = ni(2, 8)\n",
        "    a = nl(n, 2, 8)\n",
        "\n",
        "    print(n)\n",
        "    print(a)\n",
        "\n",
        "\n",
        "if __name__ == \"__main__\":\n",
        "    main()\n",
    );

    const EXPECTED_STRESS_BRUTE: &str = concat!(
        "import sys\n",
        "\n",
        "input = sys.stdin.readline\n",
        "\n",
        "\n",
        "def brute() -> None:\n",
        "    # TODO: implement a simple correct solution\n",
        "    n = int(input())\n",
        "    a = list(map(int, input().split()))\n",
        "\n",
        "    # ...\n",
        "\n",
        "    print()\n",
        "\n",
        "\n",
        "if __name__ == \"__main__\":\n",
        "    brute()\n",
    );

    #[test]
    fn selects_template_for_each_language() {
        let cpp = builtin_template(Language::Cpp);
        let python = builtin_template(Language::Python);

        assert!(cpp.contains("#include <bits/stdc++.h>"));
        assert!(python.contains("def main():"));
        assert!(cpp.ends_with('\n'));
        assert!(python.ends_with('\n'));
        assert_ne!(cpp, python);
    }

    #[test]
    fn embeds_the_exact_protocol_stress_templates() {
        let generator = stress_generator_template();
        let brute = stress_brute_template();

        assert_eq!(generator.as_bytes(), EXPECTED_STRESS_GENERATOR.as_bytes());
        assert_eq!(brute.as_bytes(), EXPECTED_STRESS_BRUTE.as_bytes());

        for template in [generator, brute] {
            assert!(template.ends_with('\n'));
            assert!(!template.ends_with("\n\n"));
            assert!(!template.as_bytes().contains(&b'\r'));
        }

        assert!(generator.contains("sys.argv[1]"));
    }
}
