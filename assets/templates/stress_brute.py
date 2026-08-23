import sys

input = sys.stdin.readline


def ni() -> int:
    return int(input())

def nm():
    return map(int, input().split())

def nl() -> list[int]:
    return list(nm())

def si() -> str:
    return input().strip()


def brute() -> None:
    # TODO: implement a simple correct solution
    n = ni()
    a = nl()

    # ...

    print()


if __name__ == "__main__":
    brute()
