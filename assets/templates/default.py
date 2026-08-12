import sys
from collections import deque, defaultdict, Counter
from heapq import heappop, heappush, heapify
from itertools import product, combinations, accumulate, permutations, groupby
from math import sqrt, isqrt, comb, gcd

input = sys.stdin.readline
sys.setrecursionlimit(10**7)

DIR4 = [(1,0),(-1,0),(0,1),(0,-1)]
DIR8 = [(1,0),(-1,0),(0,1),(0,-1),(1,1),(1,-1),(-1,1),(-1,-1)]

INF = 10**30
YES,NO = "Yes", "No"
MOD = 10**9 + 7
MOD2  = 998244353

def ni(): return int(input())
def nm(): return map(int,input().split())
def nl(): return list(nm())

def si(): return input().strip()
def sm(): return si().split()
def sl(): return list(si())


def main():
    n = ni()
    a = nl()

    print()

if __name__ == '__main__':
    main()
