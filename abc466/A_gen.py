import random
import sys
import string

def ni(lo: int, hi: int) -> int:
    return random.randint(lo, hi)

def nl(amount: int, lo: int, hi: int) -> None:
    return " ".join(str(ni(lo, hi)) for _ in range(amount))

def si(length: int = 1, chars: str = string.ascii_lowercase) -> str:
    return "".join(random.choice(chars) for _ in range(length))


def main() -> None:
    seed = int(sys.argv[1]) if len(sys.argv) >= 2 else None
    random.seed(seed)

    # TODO: replace with brute force solution
    n = ni(2, 8)
    a = nl(n, -100, 100)

    print(n)
    print(a)


if __name__ == "__main__":
    main()
