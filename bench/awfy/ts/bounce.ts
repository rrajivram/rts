// Typed port of the are-we-fast-yet benchmark of the same name (MIT license; see ../LICENSE.md).
import { Benchmark } from './benchmark.ts';
import { Random } from './som.ts';

class Ball {
  x: number;
  y: number;
  xVel: number;
  yVel: number;

  constructor(random: Random) {
    this.x = random.next() % 500;
    this.y = random.next() % 500;
    this.xVel = (random.next() % 300) - 150;
    this.yVel = (random.next() % 300) - 150;
  }

  bounce(): boolean {
    const xLimit = 500;
    const yLimit = 500;
    let bounced = false;
    this.x += this.xVel;
    this.y += this.yVel;
    if (this.x > xLimit) {
      this.x = xLimit; this.xVel = 0 - Math.abs(this.xVel); bounced = true;
    }
    if (this.x < 0) {
      this.x = 0; this.xVel = Math.abs(this.xVel); bounced = true;
    }
    if (this.y > yLimit) {
      this.y = yLimit; this.yVel = 0 - Math.abs(this.yVel); bounced = true;
    }
    if (this.y < 0) {
      this.y = 0; this.yVel = Math.abs(this.yVel); bounced = true;
    }
    return bounced;
  }
}

class Bounce extends Benchmark {
  benchmark(): number {
    const random = new Random();
    const ballCount = 100;
    let bounces = 0;
    const balls: Ball[] = [];
    for (let i = 0; i < ballCount; i += 1) {
      balls.push(new Ball(random));
    }
    for (let i = 0; i < 50; i += 1) {
      for (const ball of balls) {
        if (ball.bounce()) {
          bounces += 1;
        }
      }
    }
    return bounces;
  }

  verifyResult(result: number): boolean {
    return result === 1331;
  }
}

export const newInstance = (): Benchmark => new Bounce();
