import random
import string
import sys


def ni(lo: int, hi: int) -> int:
    return random.randint(lo, hi)

def nl(amount: int, lo: int, hi: int) -> str:
    return " ".join(str(ni(lo, hi)) for _ in range(amount))

def si(length: int = 1, chars: str = string.ascii_lowercase) -> str:
    return "".join(random.choice(chars) for _ in range(length))


def main() -> None:
    seed = int(sys.argv[1])
    random.seed(seed)

    # TODO: generate one test case
    n = ni(2, 8)
    a = nl(n, 2, 8)

    print(n)
    print(a)


if __name__ == "__main__":
    main()
