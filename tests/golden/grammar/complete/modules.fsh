import './lib/math.fsh'
let answer = 42
def add(left, right) { return $left + $right }
export { answer, add }
import { answer, add } from './lib/math.fsh'
